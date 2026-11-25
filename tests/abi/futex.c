// Futex stress: lost wakeups show up as a stall.
//   futex [seconds per test]
// 1. a pool of worker threads takes items from a queue guarded by a mutex and a condition variable
//    (pthread_cond_signal per item, as V8's and libuv's thread pools do);
// 2. ping-pong between pairs of threads through condition variables;
// 3. many threads contending for one mutex.
// Each test runs for a fixed time; if no item moves for 2 s, it prints "futex: STUCK ..." and fails.
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static double secs = 1.0;
static int failures;

// --- 1. thread pool
#define WORKERS 8
static pthread_mutex_t qm = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t qc = PTHREAD_COND_INITIALIZER, dc = PTHREAD_COND_INITIALIZER;
static long queued, done_items;
static int stop_pool;

static void *pool_worker(void *a) {
    (void)a;
    pthread_mutex_lock(&qm);
    for (;;) {
        while (queued == 0 && !stop_pool)
            pthread_cond_wait(&qc, &qm);
        if (queued == 0 && stop_pool)
            break;
        queued--;
        pthread_mutex_unlock(&qm);
        // a little work outside the lock
        volatile int x = 0;
        for (int i = 0; i < 200; i++) x += i;
        pthread_mutex_lock(&qm);
        done_items++;
        pthread_cond_signal(&dc);
    }
    pthread_mutex_unlock(&qm);
    return 0;
}

static void test_pool(void) {
    pthread_t t[WORKERS];
    for (int i = 0; i < WORKERS; i++) pthread_create(&t[i], 0, pool_worker, 0);
    long posted = 0;
    double end = now() + secs;
    while (now() < end) {
        // post a burst, then wait until it is done (like requests arriving and being answered)
        int burst = 1 + rand() % 16;
        pthread_mutex_lock(&qm);
        for (int i = 0; i < burst; i++) {
            queued++;
            pthread_cond_signal(&qc);
        }
        posted += burst;
        while (done_items < posted) {
            struct timespec ts;
            clock_gettime(CLOCK_REALTIME, &ts);
            ts.tv_sec += 2;
            if (pthread_cond_timedwait(&dc, &qm, &ts) != 0 && done_items < posted) {
                printf("futex: STUCK pool: posted %ld done %ld queued %ld\n", posted, done_items, queued);
                failures++;
                pthread_mutex_unlock(&qm);
                return; // leave the threads
            }
        }
        pthread_mutex_unlock(&qm);
    }
    pthread_mutex_lock(&qm);
    stop_pool = 1;
    pthread_cond_broadcast(&qc);
    pthread_mutex_unlock(&qm);
    for (int i = 0; i < WORKERS; i++) pthread_join(t[i], 0);
    printf("ok    pool: %ld items through %d workers\n", posted, WORKERS);
}

// --- 2. ping-pong
#define PAIRS 4
struct pair {
    pthread_mutex_t m;
    pthread_cond_t c;
    long turn; // even: ping's turn, odd: pong's
    int stop;
};
static struct pair pairs[PAIRS];
static _Atomic long pp_total;

static void *ponger(void *a) {
    struct pair *p = a;
    pthread_mutex_lock(&p->m);
    while (!p->stop) {
        while (p->turn % 2 == 0 && !p->stop) pthread_cond_wait(&p->c, &p->m);
        if (p->stop) break;
        p->turn++;
        pthread_cond_signal(&p->c);
    }
    pthread_mutex_unlock(&p->m);
    return 0;
}

static void *pinger(void *a) {
    struct pair *p = a;
    double end = now() + secs;
    pthread_mutex_lock(&p->m);
    while (now() < end) {
        p->turn++;
        pthread_cond_signal(&p->c);
        while (p->turn % 2 == 1) {
            struct timespec ts;
            clock_gettime(CLOCK_REALTIME, &ts);
            ts.tv_sec += 2;
            if (pthread_cond_timedwait(&p->c, &p->m, &ts) != 0 && p->turn % 2 == 1) {
                printf("futex: STUCK ping-pong at turn %ld\n", p->turn);
                failures++;
                p->stop = 1;
                pthread_mutex_unlock(&p->m);
                return 0;
            }
        }
        pp_total++;
    }
    p->stop = 1;
    pthread_cond_broadcast(&p->c);
    pthread_mutex_unlock(&p->m);
    return 0;
}

static void test_pingpong(void) {
    pthread_t a[PAIRS], b[PAIRS];
    for (int i = 0; i < PAIRS; i++) {
        pthread_mutex_init(&pairs[i].m, 0);
        pthread_cond_init(&pairs[i].c, 0);
        pthread_create(&b[i], 0, ponger, &pairs[i]);
        pthread_create(&a[i], 0, pinger, &pairs[i]);
    }
    for (int i = 0; i < PAIRS; i++) {
        pthread_join(a[i], 0);
        pthread_join(b[i], 0);
    }
    printf("ok    ping-pong: %ld round trips over %d pairs\n", (long)pp_total, PAIRS);
}

// --- 3. mutex contention
#define CONTENDERS 16
static pthread_mutex_t cm = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static _Atomic int cstop;

static void *contender(void *a) {
    long n = 0;
    while (!cstop) {
        pthread_mutex_lock(&cm);
        counter++;
        pthread_mutex_unlock(&cm);
        n++;
    }
    *(long *)a = n;
    return 0;
}

static void test_mutex(void) {
    pthread_t t[CONTENDERS];
    long n[CONTENDERS];
    for (int i = 0; i < CONTENDERS; i++) pthread_create(&t[i], 0, contender, &n[i]);
    long last = -1;
    double end = now() + secs, last_move = now();
    while (now() < end) {
        usleep(100000);
        pthread_mutex_lock(&cm);
        long c = counter;
        pthread_mutex_unlock(&cm);
        if (c != last) last_move = now();
        else if (now() - last_move > 2) {
            printf("futex: STUCK mutex at %ld\n", c);
            failures++;
            return;
        }
        last = c;
    }
    cstop = 1;
    long sum = 0;
    for (int i = 0; i < CONTENDERS; i++) {
        pthread_join(t[i], 0);
        sum += n[i];
    }
    if (sum != counter) {
        printf("futex: FAIL mutex: %ld increments, counter %ld\n", sum, counter);
        failures++;
        return;
    }
    printf("ok    mutex: %ld increments by %d threads\n", counter, CONTENDERS);
}

int main(int argc, char **argv) {
    if (argc > 1) secs = atof(argv[1]);
    setvbuf(stdout, 0, _IOLBF, 0);
    test_pool();
    test_pingpong();
    test_mutex();
    printf("futex: %d failures\n", failures);
    return failures != 0;
}
