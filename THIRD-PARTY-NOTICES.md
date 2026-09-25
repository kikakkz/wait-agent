# Third-Party Notices

WaitAgent vendors or downloads third-party software at runtime. This file
collects the notices for that software.

## MSYS2 runtime (Windows)

On Windows, WaitAgent downloads and provisions a fixed-version
[MSYS2](https://www.msys2.org/) base environment
(`msys2-base-x86_64-20240727.tar.xz`, fetched from
[repo.msys2.org](https://repo.msys2.org/) or one of its mirrors) into
`%LOCALAPPDATA%\waitagent\msys64`, and then installs the `openssh` and `git`
packages with the environment's own `pacman`.

- Source: <https://repo.msys2.org/distrib/x86_64/msys2-base-x86_64-20240727.tar.xz>
- Licenses: MSYS2 and the packages it contains are a mix of BSD, MIT and GPL
  licenses (the MSYS2 project itself is BSD-licensed; individual packages carry
  their own upstream licenses). See the MSYS2 project's license page:
  <https://www.msys2.org/license/>

The runtime is cached on disk and can be removed by deleting the
`%LOCALAPPDATA%\waitagent` directory.

## Rust crates

WaitAgent is built on the Rust crate ecosystem; the full list with versions and
licenses is recorded in [Cargo.lock](Cargo.lock) and each crate's license file
in the cargo registry.
