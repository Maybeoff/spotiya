#include <stdio.h>
#include <unistd.h>

void spotiya_native_stderr(void) {
#if defined(__GLIBC__)
    /* Vendor ALSA prints even in its constructors/destructors. Rust writes
       directly to fd 1; redirect only libc's buffered stdout stream. */
    stdout->_fileno = STDERR_FILENO;
    (void)fflush(stdout);
    (void)setvbuf(stdout, NULL, _IOLBF, 0);
#endif
}
