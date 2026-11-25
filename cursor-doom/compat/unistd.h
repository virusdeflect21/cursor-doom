/* Minimal <unistd.h> shim so doomgeneric's UNIX-flavoured sources build with
 * MSVC. The engine only includes the header for historical reasons; it does not
 * call POSIX-only functions that matter for this port. */
#ifndef CURSOR_DOOM_COMPAT_UNISTD_H
#define CURSOR_DOOM_COMPAT_UNISTD_H

#include <stdlib.h>
#include <stdio.h>
#include <io.h>
#include <process.h>
#include <direct.h>
#include <windows.h>

#define getpid _getpid
#define unlink _unlink
#define getcwd _getcwd
#define access _access
#define isatty _isatty
#define fileno _fileno
#define read _read
#define write _write
#define close _close

#ifndef _SLEEP_DEFINED_
static __inline void usleep(unsigned long usec) { Sleep((DWORD)(usec / 1000)); }
#define _SLEEP_DEFINED_
#endif

#endif /* CURSOR_DOOM_COMPAT_UNISTD_H */
