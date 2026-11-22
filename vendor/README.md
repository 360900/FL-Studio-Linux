# Bundled audio driver

These binaries are installed into the private Wine build and registered in the
FL Studio prefix. They are not loaded on the host.

## WinePipe

- Source: https://github.com/mrquantumoff/winepipe
- Commit: `7236b5368ffb70eaebc9417307e61cc114af6e42`
- Version: 1.0.2 workspace
- License: BSD-2-Clause, see `winepipe/LICENSE`
- Build: `make driver` with Rust 1.99, the
  `x86_64-pc-windows-gnu` and `i686-pc-windows-gnu` targets, mingw-w64 GCC
  16.2, and `winebuild` from Wine 11.22.

SHA-256:

```
706bd16706fec89e0c3d32151ba47cea02ff0bd8d463bfac9d8201bfa7217237  winepipe.so
085f3a897162a85968162c6ff9d32ea9dcd88f6ad13c47e888846a99690dd0f0  winepipe-x86_64.dll
cba2293424df3b11016b64f2833d626b0b18975eba7e065d4af66146dd1e6a37  winepipe-i386.dll
```
