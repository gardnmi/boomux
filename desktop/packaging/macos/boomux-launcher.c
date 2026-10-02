/* Native entry point: keep the bundle's main signature in Mach-O, not xattrs.
 * Startup/environment policy remains in the bounded Rust bootstrap. */
#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef __APPLE__
#include <mach-o/dyld.h>
#endif

int main(int argc, char **argv) {
    char executable[4096];
#ifdef __APPLE__
    uint32_t capacity = sizeof(executable);
    if (_NSGetExecutablePath(executable, &capacity) != 0) {
        fputs("Boomux launcher path exceeds limit\n", stderr);
        return 1;
    }
#elif defined(BOOMUX_LAUNCHER_TEST)
    ssize_t length = readlink("/proc/self/exe", executable, sizeof(executable) - 1);
    if (length < 0 || (size_t)length >= sizeof(executable) - 1) return 1;
    executable[length] = '\0';
#else
#error "The application launcher is macOS-only"
#endif
    char *slash = strrchr(executable, '/');
    if (slash == NULL) return 1;
    const char sibling[] = "/boomux-desktop";
    size_t prefix = (size_t)(slash - executable);
    if (prefix + sizeof(sibling) > sizeof(executable)) return 1;
    memcpy(executable + prefix, sibling, sizeof(sibling));
    char **arguments = calloc((size_t)argc + 2, sizeof(char *));
    if (arguments == NULL) return 1;
    arguments[0] = executable;
    arguments[1] = "--macos-launch";
    for (int i = 1; i < argc; i++) arguments[i + 1] = argv[i];
    execv(executable, arguments);
    fprintf(stderr, "Could not open the bundled Boomux Desktop: %s\n", strerror(errno));
    free(arguments);
#ifdef __APPLE__
    /* Fixed text only; user paths/arguments never become AppleScript source. */
    execl("/usr/bin/osascript", "osascript", "-e",
          "display alert \"Boomux could not open\" message \"The bundled Desktop executable is missing or could not start. Reinstall the complete app and try again.\" as critical",
          (char *)NULL);
#endif
    return 1;
}
