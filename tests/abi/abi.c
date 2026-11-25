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
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/mman.h>
#include <sys/socket.h>
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


// Half-close and close on a connected stream pair (a, b): shutdown(SHUT_WR) ends only one
// direction; data written before a close is still read before end-of-file.
static void stream_semantics(const char *kind, int a, int b) {
    char name[64], buf[16];
    struct pollfd p = {b, POLLIN | POLLOUT | POLLRDHUP, 0};
    snprintf(name, sizeof name, "%s-shut-wr", kind);
    CHECK(name, write(a, "req", 3) == 3 && shutdown(a, SHUT_WR) == 0, "errno %d", errno);
    snprintf(name, sizeof name, "%s-half-close-read", kind);
    CHECK(name, read(b, buf, sizeof buf) == 3 && read(b, buf, sizeof buf) == 0, "errno %d", errno);
    poll(&p, 1, 0);
    snprintf(name, sizeof name, "%s-half-close-poll", kind);
    CHECK(name, (p.revents & (POLLIN | POLLRDHUP | POLLOUT)) == (POLLIN | POLLRDHUP | POLLOUT) && !(p.revents & POLLHUP), "revents %#x", p.revents);
    snprintf(name, sizeof name, "%s-write-after-peer-shut-wr", kind);
    CHECK(name, write(b, "resp", 4) == 4, "errno %d", errno);
    snprintf(name, sizeof name, "%s-read-reply", kind);
    CHECK(name, read(a, buf, sizeof buf) == 4 && memcmp(buf, "resp", 4) == 0, "errno %d", errno);
    snprintf(name, sizeof name, "%s-write-after-shut-wr", kind);
    errno = 0;
    CHECK(name, write(a, "x", 1) == -1 && errno == EPIPE, "errno %d", errno);
    // b writes its last bytes and closes: a reads them, then end-of-file
    CHECK("stream-last-write", write(b, "bye", 3) == 3, "errno %d", errno);
    close(b);
    snprintf(name, sizeof name, "%s-data-before-eof", kind);
    CHECK(name, read(a, buf, sizeof buf) == 3 && read(a, buf, sizeof buf) == 0, "errno %d", errno);
    p.fd = a;
    p.events = POLLIN | POLLRDHUP;
    poll(&p, 1, 0);
    snprintf(name, sizeof name, "%s-closed-poll", kind);
    CHECK(name, (p.revents & (POLLIN | POLLHUP)) == (POLLIN | POLLHUP), "revents %#x", p.revents);
    close(a);
}

static void test_sockets(void) {
    signal(SIGPIPE, SIG_IGN);
    int sv[2];
    CHECK("socketpair", socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "errno %d", errno);
    stream_semantics("unix", sv[0], sv[1]);
    // loopback TCP
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in sa = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof sa;
    CHECK("tcp-listen", bind(l, (struct sockaddr *)&sa, sizeof sa) == 0 && listen(l, 1) == 0 && getsockname(l, (struct sockaddr *)&sa, &len) == 0, "errno %d", errno);
    int c = socket(AF_INET, SOCK_STREAM, 0);
    // more connections than the backlog: connect does not fail (Linux only delays it)
    int c2 = socket(AF_INET, SOCK_STREAM, 0), c3 = socket(AF_INET, SOCK_STREAM, 0);
    int rc = connect(c, (struct sockaddr *)&sa, sizeof sa);
    CHECK("tcp-connect", rc == 0, "errno %d", errno);
    if (rc != 0) return;
    int s = accept(l, NULL, NULL);
    CHECK("tcp-accept", s >= 0, "errno %d", errno);
    fcntl(c2, F_SETFL, O_NONBLOCK);
    fcntl(c3, F_SETFL, O_NONBLOCK);
    int r2 = connect(c2, (struct sockaddr *)&sa, sizeof sa), e2 = errno;
    int r3 = connect(c3, (struct sockaddr *)&sa, sizeof sa), e3 = errno;
    CHECK("tcp-connect-full-backlog", (r2 == 0 || e2 == EINPROGRESS) && (r3 == 0 || e3 == EINPROGRESS), "%d/%d %d/%d", r2, e2, r3, e3);
    close(c2);
    close(c3);
    stream_semantics("tcp", c, s);
    close(l);
}

// Namespaces and seccomp. Each case runs in a child (fork) whose exit status carries the result:
// 0 = ok, otherwise the number of the failed step.
static int in_child(int (*f)(void)) {
    pid_t p = fork();
    if (p == 0) _exit(f());
    int st = 0;
    waitpid(p, &st, 0);
    return st;
}

static int write_file(const char *path, const char *s) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
    close(fd);
    return r;
}

// whether we run in a chroot (/ is not init's root): user namespaces are refused there (EPERM)
static int chrooted(void) {
    struct stat a, b;
    if (stat("/", &a) || stat("/proc/1/root/", &b)) return 0;
    return a.st_dev != b.st_dev || a.st_ino != b.st_ino;
}

// an unprivileged user maps itself to root in a new user namespace
static int userns_case(void) {
    if (chrooted()) return unshare(CLONE_NEWUSER) == -1 && errno == EPERM ? 100 : 14;
    if (setgid(65534) || setuid(65534)) return 1;
    // changing ids made us non-dumpable (/proc/self owned by root); we may say otherwise
    if (prctl(PR_GET_DUMPABLE) != 0) return 13;
    prctl(PR_SET_DUMPABLE, 1);
    if (unshare(CLONE_NEWUSER)) return 2;
    if (write_file("/proc/self/setgroups", "deny")) return 3;
    if (write_file("/proc/self/uid_map", "0 65534 1")) return 4;
    if (write_file("/proc/self/gid_map", "0 65534 1")) return 5;
    if (getuid() != 0 || getgid() != 0) return 6;
    // mapping again is refused; mapping another id was never allowed
    if (write_file("/proc/self/uid_map", "1 65534 1") == 0) return 7;
    // root here, but files of the initial namespace's root show as the overflow id
    struct stat st;
    if (stat("/", &st) || st.st_uid != 65534) return 8;
    // a mount namespace of our own: a tmpfs only we see
    if (unshare(CLONE_NEWNS)) return 9;
    if (mount("none", "/tmp", "tmpfs", 0, "size=1m")) return 10;
    if (write_file("/proc/self/uid_map", "0 0 1") == 0) return 11;
    // not allowed to mount a block file system, nor to change a flag the parent namespace locked
    if (mount("/dev/vda", "/mnt", "ext4", 0, 0) == 0) return 12;
    return 0;
}

static int pidns_child(void) { return getpid() == 1 && getppid() == 0 ? 0 : 20; }

static int pidns_case(void) {
    if (unshare(CLONE_NEWPID)) return 1;
    pid_t p = fork();
    if (p == 0) _exit(pidns_child());
    if (p <= 1) return 2;
    int st;
    if (waitpid(p, &st, 0) != p) return 3;
    if (!WIFEXITED(st)) return 4;
    return WEXITSTATUS(st);
}

static int utsns_case(void) {
    if (unshare(CLONE_NEWUTS)) return 1;
    if (sethostname("abi-test", 8)) return 2;
    struct utsname u;
    uname(&u);
    return strcmp(u.nodename, "abi-test") == 0 ? 0 : 3;
}

static int netns_case(void) {
    if (unshare(CLONE_NEWNET)) return 1;
    struct sockaddr_in sa = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int l = socket(AF_INET, SOCK_STREAM, 0);
    // lo is down in a new namespace
    int c = socket(AF_INET, SOCK_STREAM, 0);
    sa.sin_port = htons(9);
    if (connect(c, (struct sockaddr *)&sa, sizeof sa) == 0 || errno != ENETUNREACH) return 2;
    close(c);
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "lo");
    if (ioctl(l, SIOCGIFFLAGS, &ifr)) return 3;
    ifr.ifr_flags |= IFF_UP;
    if (ioctl(l, SIOCSIFFLAGS, &ifr)) return 4;
    sa.sin_port = 0;
    socklen_t len = sizeof sa;
    if (bind(l, (struct sockaddr *)&sa, sizeof sa) || listen(l, 1) || getsockname(l, (struct sockaddr *)&sa, &len)) return 5;
    c = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(c, (struct sockaddr *)&sa, sizeof sa)) return 6;
    // only lo here
    if (if_nametoindex("eth0") != 0) return 7;
    return 0;
}

static int seccomp_errno_case(void) {
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0), // nr
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = {4, f};
    // without no_new_privs or CAP_SYS_ADMIN this is refused; root may
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return 1;
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)) return 2;
    if (prctl(PR_GET_SECCOMP) != 2) return 3;
    errno = 0;
    if (syscall(SYS_getppid) != -1 || errno != EPERM) return 4;
    if (getpid() <= 0) return 5;
    // a broken program is refused
    struct sock_filter bad[] = {BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 1)};
    struct sock_fprog bp = {1, bad};
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &bp) == 0 || errno != EINVAL) return 6;
    // the filter survives fork
    pid_t c = fork();
    if (c == 0) _exit(syscall(SYS_getppid) == -1 ? 0 : 1);
    int st;
    waitpid(c, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 7;
}

static int seccomp_kill_case(void) {
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = {4, f};
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog)) return 1;
    syscall(SYS_getppid);
    return 2; // not reached
}

static void test_namespaces(void) {
    int st = in_child(userns_case);
    // 100: in a chroot, where the kernel rightly refuses (the rest is checked outside it)
    CHECK("userns-map-mount", WIFEXITED(st) && (WEXITSTATUS(st) == 0 || WEXITSTATUS(st) == 100), "status %#x", st);
    st = in_child(pidns_case);
    CHECK("pidns-pid1", WIFEXITED(st) && WEXITSTATUS(st) == 0, "status %#x", st);
    st = in_child(utsns_case);
    struct utsname u;
    uname(&u);
    CHECK("utsns-hostname", WIFEXITED(st) && WEXITSTATUS(st) == 0 && strcmp(u.nodename, "abi-test") != 0, "status %#x, host %s", st, u.nodename);
    st = in_child(netns_case);
    CHECK("netns-lo", WIFEXITED(st) && WEXITSTATUS(st) == 0, "status %#x", st);
    st = in_child(seccomp_errno_case);
    CHECK("seccomp-errno", WIFEXITED(st) && WEXITSTATUS(st) == 0, "status %#x", st);
    st = in_child(seccomp_kill_case);
    CHECK("seccomp-kill", WIFSIGNALED(st) && WTERMSIG(st) == SIGSYS, "status %#x", st);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "child-exec") == 0) return 42;
    setvbuf(stdout, NULL, _IONBF, 0);
    test_threads();
    test_signals();
    test_memory();
    test_time();
    test_files();
    test_sockets();
    test_namespaces();
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
