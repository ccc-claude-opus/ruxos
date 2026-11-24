// ruxos ABI tests: each check prints "ok <name>" or "FAIL <name>: ..." and the program ends with
// "abi: N failures". Runs unchanged on Linux (differential testing).
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;
#define CHECK(name, cond, ...)                                                   \
    do {                                                                         \
        if (cond) printf("ok %s\n", name);                                      \
        else { printf("FAIL %s: ", name); printf(__VA_ARGS__); printf("\n"); failures++; } \
    } while (0)

static long counter;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int turn;

static void *worker(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&mu);
        counter++;
        pthread_mutex_unlock(&mu);
    }
    // round robin through a condition variable
    for (int r = 0; r < 50; r++) {
        pthread_mutex_lock(&mu);
        while (turn % 4 != id) pthread_cond_wait(&cv, &mu);
        turn++;
        pthread_cond_broadcast(&cv);
        pthread_mutex_unlock(&mu);
    }
    return (void *)(id * 10);
}

static void test_threads(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    long sum = 0;
    for (int i = 0; i < 4; i++) {
        void *r;
        pthread_join(t[i], &r);
        sum += (long)r;
    }
    CHECK("threads-mutex", counter == 80000, "counter %ld", counter);
    CHECK("threads-cond", turn == 200, "turn %d", turn);
    CHECK("threads-join", sum == 60, "sum %ld", sum);
}

static sigjmp_buf jb;
static volatile void *fault_addr;
static volatile int fault_code;
static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)sig; (void)uc;
    fault_addr = si->si_addr;
    fault_code = si->si_code;
    siglongjmp(jb, 1);
}

static volatile int alt_ok;
static char altstack[65536];
static void on_usr1(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    char here;
    alt_ok = sig == SIGUSR1 && si->si_pid == getpid() && &here >= altstack && &here < altstack + sizeof altstack;
}

static volatile int alarms;
static void on_alrm(int s) { (void)s; alarms++; }

static void test_signals(void) {
    struct sigaction sa = {0};
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);
    volatile int *p = (int *)0x1000;
    if (sigsetjmp(jb, 1) == 0) {
        *p = 1;
        CHECK("sigsegv", 0, "no fault");
    } else {
        CHECK("sigsegv", fault_addr == (void *)0x1000 && fault_code == SEGV_MAPERR, "addr %p code %d", fault_addr, fault_code);
    }
    // a read-only page: SEGV_ACCERR
    char *ro = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sigsetjmp(jb, 1) == 0) {
        ro[10] = 1;
        CHECK("sigsegv-accerr", 0, "no fault");
    } else {
        CHECK("sigsegv-accerr", fault_addr == ro + 10 && fault_code == SEGV_ACCERR, "addr %p code %d", fault_addr, fault_code);
    }
    stack_t ss = {.ss_sp = altstack, .ss_size = sizeof altstack};
    sigaltstack(&ss, NULL);
    sa.sa_sigaction = on_usr1;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGUSR1, &sa, NULL);
    kill(getpid(), SIGUSR1);
    CHECK("sigaltstack-siginfo", alt_ok, "handler not on the alternate stack or bad siginfo");
    // SA_RESTART: a blocking read survives SIGALRM
    int pp[2];
    pipe(pp);
    struct sigaction al = {0};
    al.sa_handler = on_alrm;
    al.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &al, NULL);
    if (fork() == 0) {
        usleep(300000);
        write(pp[1], "x", 1);
        _exit(0);
    }
    struct itimerval it = {.it_value = {0, 100000}};
    setitimer(ITIMER_REAL, &it, NULL);
    char c = 0;
    ssize_t n = read(pp[0], &c, 1);
    CHECK("sa-restart", n == 1 && c == 'x' && alarms == 1, "n %zd alarms %d", n, alarms);
    wait(NULL);
    // without SA_RESTART: EINTR
    al.sa_flags = 0;
    sigaction(SIGALRM, &al, NULL);
    setitimer(ITIMER_REAL, &it, NULL);
    n = read(pp[0], &c, 1);
    CHECK("eintr", n == -1 && errno == EINTR, "n %zd errno %d", n, errno);
    // blocked signals stay pending
    sigset_t s, old;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR2);
    sigprocmask(SIG_BLOCK, &s, &old);
    kill(getpid(), SIGUSR2);
    sigset_t pend;
    sigpending(&pend);
    int sig = 0;
    sigwait(&s, &sig);
    CHECK("sigpending-sigwait", sigismember(&pend, SIGUSR2) && sig == SIGUSR2, "sig %d", sig);
    sigprocmask(SIG_SETMASK, &old, NULL);
}

static void test_memory(void) {
    // anonymous memory and copy-on-write across fork
    int *a = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    a[1000] = 42;
    int *sh = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t pid = fork();
    if (pid == 0) {
        a[1000] = 7;
        sh[0] = a[1000] + 1;
        _exit(a[1000] == 7 ? 0 : 1);
    }
    int st;
    waitpid(pid, &st, 0);
    CHECK("fork-cow", WIFEXITED(st) && WEXITSTATUS(st) == 0 && a[1000] == 42, "status %x value %d", st, a[1000]);
    CHECK("shared-anon", sh[0] == 8, "shared %d", sh[0]);
    // file mapping, shared: writes reach the file
    int fd = open("/tmp/mapped", O_RDWR | O_CREAT | O_TRUNC, 0644);
    ftruncate(fd, 8192);
    char *m = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    strcpy(m + 4096, "through mmap");
    char buf[32] = {0};
    pread(fd, buf, 12, 4096);
    CHECK("mmap-shared-file", strcmp(buf, "through mmap") == 0, "read '%s'", buf);
    pwrite(fd, "via write", 9, 100);
    CHECK("mmap-coherent", memcmp(m + 100, "via write", 9) == 0, "mapping did not see the write");
    munmap(m, 8192);
    close(fd);
    // mprotect and munmap
    char *r = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    r[5000] = 3;
    mprotect(r + 4096, 4096, PROT_READ);
    CHECK("mprotect-keeps-data", r[5000] == 3, "data lost");
    munmap(r + 4096, 4096);
    if (sigsetjmp(jb, 1) == 0) {
        r[5000] = 1;
        CHECK("munmap", 0, "no fault");
    } else {
        CHECK("munmap", fault_addr == r + 5000, "addr %p", fault_addr);
    }
    // brk/malloc of a lot of memory, touched
    size_t big = 64 << 20;
    char *b = malloc(big);
    memset(b, 1, big);
    long s = 0;
    for (size_t i = 0; i < big; i += 4096) s += b[i];
    CHECK("malloc-64m", s == (long)(big / 4096), "sum %ld", s);
    free(b);
    // MAP_FIXED over an existing mapping, mremap
    char *x = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    x[0] = 9;
    char *y = mremap(x, 8192, 1 << 20, MREMAP_MAYMOVE);
    CHECK("mremap", y != MAP_FAILED && y[0] == 9, "mremap %p", y);
}

static void test_time(void) {
    struct timespec a, b, r;
    clock_gettime(CLOCK_MONOTONIC, &a);
    struct timespec d = {0, 50 * 1000 * 1000};
    nanosleep(&d, NULL);
    clock_gettime(CLOCK_MONOTONIC, &b);
    long ns = (b.tv_sec - a.tv_sec) * 1000000000L + (b.tv_nsec - a.tv_nsec);
    CHECK("nanosleep", ns >= 50000000 && ns < 500000000, "slept %ld ns", ns);
    clock_gettime(CLOCK_REALTIME, &r);
    CHECK("realtime", r.tv_sec > 1700000000, "realtime %ld", (long)r.tv_sec);
    struct timeval tv;
    gettimeofday(&tv, NULL);
    CHECK("gettimeofday", tv.tv_sec >= r.tv_sec && tv.tv_sec - r.tv_sec < 5, "tv %ld", (long)tv.tv_sec);
    // the clock never goes backwards across threads/CPUs
    long back = 0;
    struct timespec prev = {0};
    for (int i = 0; i < 100000; i++) {
        struct timespec t;
        clock_gettime(CLOCK_MONOTONIC, &t);
        if (t.tv_sec < prev.tv_sec || (t.tv_sec == prev.tv_sec && t.tv_nsec < prev.tv_nsec)) back++;
        prev = t;
    }
    CHECK("monotonic", back == 0, "%ld steps back", back);
}

static void test_files(void) {
    mkdir("/tmp/d", 0755);
    for (int i = 0; i < 100; i++) {
        char n[64];
        snprintf(n, sizeof n, "/tmp/d/f%03d", i);
        close(open(n, O_CREAT | O_WRONLY, 0600));
    }
    DIR *d = opendir("/tmp/d");
    int count = 0;
    struct dirent *e;
    while ((e = readdir(d))) count++;
    closedir(d);
    CHECK("readdir", count == 102, "%d entries", count);
    // unlink while reading the directory (rm -r does this)
    d = opendir("/tmp/d");
    int removed = 0;
    while ((e = readdir(d))) {
        if (e->d_name[0] == 'f') {
            char n[300];
            snprintf(n, sizeof n, "/tmp/d/%s", e->d_name);
            removed += unlink(n) == 0;
        }
    }
    closedir(d);
    CHECK("readdir-unlink", removed == 100 && rmdir("/tmp/d") == 0, "removed %d", removed);
    struct stat st;
    int fd = open("/tmp/s", O_CREAT | O_RDWR | O_EXCL, 0640);
    write(fd, "12345", 5);
    fstat(fd, &st);
    CHECK("fstat", S_ISREG(st.st_mode) && (st.st_mode & 0777) == 0640 && st.st_size == 5, "mode %o size %ld", st.st_mode, (long)st.st_size);
    CHECK("o-excl", open("/tmp/s", O_CREAT | O_EXCL | O_RDWR, 0640) == -1 && errno == EEXIST, "errno %d", errno);
    int fd2 = dup(fd);
    lseek(fd2, 1, SEEK_SET);
    char c;
    read(fd, &c, 1);
    CHECK("dup-shares-offset", c == '2', "read %c", c);
    // poll on a pipe
    int pp[2];
    pipe(pp);
    struct pollfd pf = {.fd = pp[0], .events = POLLIN};
    int r0 = poll(&pf, 1, 10);
    write(pp[1], "z", 1);
    int r1 = poll(&pf, 1, 1000);
    CHECK("poll", r0 == 0 && r1 == 1 && (pf.revents & POLLIN), "r0 %d r1 %d", r0, r1);
    close(pp[1]);
    read(pp[0], &c, 1);
    CHECK("pipe-eof", read(pp[0], &c, 1) == 0, "no EOF");
    // POSIX record locks between processes
    int lf = open("/tmp/lock", O_CREAT | O_RDWR, 0600);
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
    fcntl(lf, F_SETLK, &fl);
    pid_t pid = fork();
    if (pid == 0) {
        int f2 = open("/tmp/lock", O_RDWR);
        struct flock l2 = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 5, .l_len = 1};
        int busy = fcntl(f2, F_SETLK, &l2) == -1 && (errno == EAGAIN || errno == EACCES);
        struct flock g = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1};
        fcntl(f2, F_GETLK, &g);
        _exit(busy && g.l_type == F_WRLCK && g.l_pid == getppid() ? 0 : 1);
    }
    int st2;
    waitpid(pid, &st2, 0);
    CHECK("fcntl-locks", WIFEXITED(st2) && WEXITSTATUS(st2) == 0, "status %x", st2);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "child-exec") == 0) return 42;
    setvbuf(stdout, NULL, _IONBF, 0);
    test_threads();
    test_signals();
    test_memory();
    test_time();
    test_files();
    pid_t pid = fork();
    if (pid == 0) {
        execl(argv[0], argv[0], "child-exec", (char *)NULL);
        _exit(1);
    }
    int st;
    waitpid(pid, &st, 0);
    CHECK("exec-wait", WIFEXITED(st) && WEXITSTATUS(st) == 42, "status %x", st);
    printf("abi: %d failures\n", failures);
    return failures != 0;
}
