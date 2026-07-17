/* Force-included ONLY when compiling ft8_lib's vendored C sources for the
 * x86_64-pc-windows-gnu (MinGW) target — see build.rs.
 *
 * MinGW's <string.h> does not reliably expose a prototype for `stpcpy` on
 * current toolchain versions, and GCC 14+ treats an implicit function
 * declaration as a hard compile error. Worse than a build break: an
 * implicit declaration defaults to `int stpcpy()`, which on a 64-bit
 * target truncates the real `char *` return value — a correctness bug,
 * not just a diagnostic. This header supplies the real, correct
 * prototype so the compiler never falls back to an implicit one.
 *
 * This file is ours (not part of the vendored ft8_lib submodule) — it
 * only declares a standard C library function's signature, it contains
 * no logic derived from any external source.
 */
#ifndef PANCETTA_FT8_MINGW_STPCPY_SHIM_H
#define PANCETTA_FT8_MINGW_STPCPY_SHIM_H

#if defined(_WIN32) && !defined(__cplusplus)
char *stpcpy(char *dst, const char *src);
#endif

#endif
