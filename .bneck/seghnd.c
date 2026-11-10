#define _GNU_SOURCE
#include <signal.h>
#include <execinfo.h>
#include <ucontext.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

/* Crash forensics without a debugger: print the faulting instruction pointer,
   the mapping it lives in, and a scanned-stack trace of every return address
   that points into an executable mapping. Frame-pointer-less release builds
   break backtrace(3) after two frames, so the scan is the usable evidence. */
static unsigned long rip, rsp;

static int in_text(unsigned long a, char *out, int ol, const char **fname, unsigned long *fbase) {
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    if (!f) return 0;
    while (fgets(line, sizeof line, f)) {
        unsigned long s, e, off;
        char perm[8], path[300];
        if (sscanf(line, "%lx-%lx %7s %lx %*s %*s %299[^\n]", &s, &e, perm, &off, path) == 5) {
            if (a >= s && a < e && perm[0] == 'x' && path[0] == '/') {
                if (out) snprintf(out, ol, "%s!0x%lx", path, a - s + off);
                if (fname) { *fname = strdup(path); }
                if (fbase) { *fbase = s; }
                fclose(f);
                return 1;
            }
        }
    }
    fclose(f);
    return 0;
}

static void h(int sig, siginfo_t *si, void *uc) {
    (void)sig; (void)si;
    rip = ((ucontext_t *)uc)->uc_mcontext.gregs[REG_RIP];
    rsp = ((ucontext_t *)uc)->uc_mcontext.gregs[REG_RSP];
    char desc[400];
    in_text(rip, desc, sizeof desc, 0, 0);
    fprintf(stderr, "\n*** SIGSEGV: rip=%s tid=%d ***\n", desc, gettid());
    /* scan 4 KiB of the stack for plausible return addresses */
    unsigned long *p = (unsigned long *)rsp;
    int found = 0;
    for (int i = 0; i < 512 && found < 24; i++, p++) {
        unsigned long a = *p;
        if (a < 0x1000) continue;
        if (in_text(a, desc, sizeof desc, 0, 0)) {
            fprintf(stderr, "  [%03d] %s\n", i, desc);
            found++;
        }
    }
    fflush(stderr);
    _exit(139);
}

__attribute__((constructor)) void inst(void) {
    struct sigaction sa;
    sa.sa_sigaction = h;
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, 0);
}
