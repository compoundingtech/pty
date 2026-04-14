/*
 * FDA wrapper for the pty supervisor.
 * Compiled during `pty supervisor launchd install` with paths and PATH
 * baked in via -D flags. Grant this binary Full Disk Access so the
 * supervisor (and sessions it spawns) can access external/removable volumes.
 *
 * Usage:
 *   pty-supervisor           Run the supervisor (exec node with bundle)
 *   pty-supervisor --check   Validate FDA, node binary, bundle file, and PATH
 *
 * Compile:
 *   cc -O2 -o pty-supervisor \
 *     -DNODE_PATH='"/path/to/node"' \
 *     -DBUNDLE_PATH='"/path/to/supervisor.bundle.js"' \
 *     -DUSER_PATH='"..."' \
 *     scripts/supervisor-wrapper.c
 */
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef NODE_PATH
#error "NODE_PATH must be defined at compile time"
#endif

#ifndef BUNDLE_PATH
#error "BUNDLE_PATH must be defined at compile time"
#endif

#ifndef USER_PATH
#error "USER_PATH must be defined at compile time"
#endif

/* Test FDA by reading a TCC-protected file */
static int check_fda(void) {
    const char *home = getenv("HOME");
    if (!home) {
        fprintf(stderr, "  ✗ HOME not set\n");
        return 0;
    }
    char path[1024];
    snprintf(path, sizeof(path), "%s/Library/Safari/History.db", home);
    if (access(path, R_OK) == 0) {
        return 1;
    }
    return 0;
}

static int check_file(const char *label, const char *path) {
    if (access(path, R_OK) == 0) {
        printf("  ✓ %s: %s\n", label, path);
        return 1;
    }
    fprintf(stderr, "  ✗ %s not found: %s\n", label, path);
    return 0;
}

static int check_executable(const char *label, const char *path) {
    if (access(path, X_OK) == 0) {
        printf("  ✓ %s: %s\n", label, path);
        return 1;
    }
    fprintf(stderr, "  ✗ %s not executable: %s\n", label, path);
    return 0;
}

int main(int argc, char *argv[]) {
    /* Set PATH before anything else so child processes can find commands */
    setenv("PATH", USER_PATH, 1);

    int check_mode = 0;
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--check") == 0) {
            check_mode = 1;
        }
    }

    if (check_mode) {
        printf("pty supervisor wrapper check\n\n");
        int ok = 1;

        if (!check_executable("node", NODE_PATH)) ok = 0;
        if (!check_file("bundle", BUNDLE_PATH)) ok = 0;
        printf("  ✓ PATH: set (%zu chars)\n", strlen(USER_PATH));

        if (check_fda()) {
            printf("  ✓ Full Disk Access: granted\n");
        } else {
            fprintf(stderr, "  ✗ Full Disk Access: not granted\n");
            fprintf(stderr, "\n");
            fprintf(stderr, "Grant Full Disk Access to this binary:\n");
            fprintf(stderr, "  System Settings > Privacy & Security > Full Disk Access\n");
            fprintf(stderr, "  Add: %s\n", argv[0]);
            ok = 0;
        }

        printf("\n");
        if (ok) {
            printf("All checks passed.\n");
            return 0;
        } else {
            fprintf(stderr, "Some checks failed.\n");
            return 1;
        }
    }

    /* Normal mode: validate before exec */
    if (access(NODE_PATH, X_OK) != 0) {
        fprintf(stderr, "[pty-supervisor] node not found: %s\n", NODE_PATH);
        return 1;
    }
    if (access(BUNDLE_PATH, R_OK) != 0) {
        fprintf(stderr, "[pty-supervisor] bundle not found: %s\n", BUNDLE_PATH);
        return 1;
    }
    if (!check_fda()) {
        fprintf(stderr, "[pty-supervisor] Full Disk Access not granted.\n");
        fprintf(stderr, "[pty-supervisor] Grant FDA to: %s\n", argv[0]);
        fprintf(stderr, "[pty-supervisor] System Settings > Privacy & Security > Full Disk Access\n");
        return 1;
    }

    char *exec_argv[] = { NODE_PATH, BUNDLE_PATH, NULL };
    execv(NODE_PATH, exec_argv);
    /* execv only returns on error */
    perror("[pty-supervisor] execv");
    return 1;
}
