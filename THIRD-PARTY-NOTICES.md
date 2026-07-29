# Third-Party Notices

Gal is distributed under the MIT License (see LICENSE). Binary distributions
of Gal include the following third-party components. All are permissive and
compatible with MIT redistribution.

One crate (`r-efi`) offers `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. That is a
disjunction, so we elect **MIT**; no copyleft obligation attaches. It is also a
UEFI target crate and is not compiled into a normal native build. No other crate
in the tree is copyleft-licensed under any election.

Regenerate this file with `cargo metadata` after changing dependencies.

## SQLite

Gal builds `rusqlite` with the `bundled` feature, which compiles the SQLite
amalgamation directly into the binary. **SQLite is in the public domain** and
requires no attribution or license notice. It is listed here only so that
anyone auditing the binary knows it is present.

## Rust crates

### (MIT OR Apache-2.0) AND Unicode-3.0

- unicode-ident 1.0.24

### 0BSD OR MIT OR Apache-2.0

- adler2 2.0.1

### Apache-2.0

- sync_wrapper 1.0.2

### Apache-2.0 OR BSL-1.0

- ryu 1.0.23

### Apache-2.0 OR MIT

- atomic-waker 1.1.2
- base64ct 1.8.3
- fastrand 2.5.0
- idna_adapter 1.2.2
- pin-project-lite 0.2.17
- utf8_iter 1.0.4
- uuid 1.24.0

### Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT

- linux-raw-sys 0.12.1
- rustix 1.1.4
- wasi 0.11.1+wasi-snapshot-preview1
- wasip2 1.0.4+wasi-0.2.12
- wit-bindgen 0.57.1

### BSD-2-Clause OR Apache-2.0 OR MIT

- zerocopy 0.8.55
- zerocopy-derive 0.8.55

### BSD-3-Clause

- subtle 2.6.1

### MIT

- axum 0.8.9
- axum-core 0.5.6
- bytes 1.12.1
- dashmap 6.2.1
- data-encoding 2.11.0
- generic-array 0.14.7
- http-body 1.1.0
- http-body-util 0.1.4
- hyper 1.11.0
- hyper-util 0.1.20
- libsqlite3-sys 0.30.1
- matchers 0.2.0
- mio 1.2.2
- nu-ansi-term 0.50.3
- r2d2_sqlite 0.25.0
- redox_syscall 0.5.18
- rusqlite 0.32.1
- sharded-slab 0.1.7
- simd-adler32 0.3.10
- slab 0.4.12
- synstructure 0.13.2
- tokio 1.53.1
- tokio-macros 2.7.1
- tokio-tungstenite 0.29.0
- tokio-util 0.7.19
- tower 0.5.3
- tower-http 0.6.11
- tower-layer 0.3.3
- tower-service 0.3.3
- tracing 0.1.44
- tracing-attributes 0.1.31
- tracing-core 0.1.36
- tracing-log 0.2.0
- tracing-subscriber 0.3.23
- try-lock 0.2.5
- valuable 0.1.1
- want 0.3.1
- zmij 1.0.23

### MIT AND BSD-3-Clause

- matchit 0.8.4

### MIT OR Apache-2.0

- ahash 0.8.12
- anyhow 1.0.104
- argon2 0.5.3
- async-compression 0.4.42
- base64 0.22.1
- bitflags 2.13.1
- blake2 0.10.6
- block-buffer 0.10.4
- bumpalo 3.20.3
- cc 1.4.0
- cfg-if 1.0.4
- chacha20 0.10.1
- compression-codecs 0.4.38
- compression-core 0.4.32
- cpufeatures 0.2.17
- cpufeatures 0.3.0
- crc32fast 1.5.0
- crossbeam-utils 0.8.22
- crypto-common 0.1.7
- digest 0.10.7
- displaydoc 0.2.7
- errno 0.3.14
- find-msvc-tools 0.1.9
- flate2 1.1.9
- form_urlencoded 1.2.2
- futures 0.3.33
- futures-channel 0.3.33
- futures-core 0.3.33
- futures-executor 0.3.33
- futures-io 0.3.33
- futures-macro 0.3.33
- futures-sink 0.3.33
- futures-task 0.3.33
- futures-util 0.3.33
- getrandom 0.2.17
- getrandom 0.3.4
- getrandom 0.4.3
- hashbrown 0.14.5
- hashlink 0.9.1
- http 1.4.2
- httparse 1.10.1
- httpdate 1.0.3
- idna 1.1.0
- ipnet 2.12.0
- itoa 1.0.18
- js-sys 0.3.103
- lazy_static 1.5.0
- libc 0.2.189
- lock_api 0.4.14
- log 0.4.33
- mime 0.3.17
- once_cell 1.21.4
- parking_lot 0.12.5
- parking_lot_core 0.9.12
- password-hash 0.5.0
- percent-encoding 2.3.2
- pkg-config 0.3.33
- ppv-lite86 0.2.21
- proc-macro2 1.0.107
- quote 1.0.47
- rand 0.10.2
- rand 0.8.7
- rand 0.9.5
- rand_chacha 0.3.1
- rand_chacha 0.9.0
- rand_core 0.10.1
- rand_core 0.6.4
- rand_core 0.9.5
- regex-automata 0.4.16
- regex-syntax 0.8.11
- reqwest 0.12.28
- rustversion 1.0.23
- scopeguard 1.2.0
- serde 1.0.229
- serde_core 1.0.229
- serde_derive 1.0.229
- serde_json 1.0.151
- serde_path_to_error 0.1.20
- sha1 0.10.7
- sha2 0.10.9
- shlex 2.0.1
- signal-hook-registry 1.4.8
- smallvec 1.15.2
- socket2 0.6.5
- stable_deref_trait 1.2.1
- syn 2.0.119
- syn 3.0.3
- tempfile 3.27.0
- thiserror 2.0.19
- thiserror-impl 2.0.19
- thread_local 1.1.10
- tungstenite 0.29.0
- typenum 1.20.1
- url 2.5.8
- wasm-bindgen 0.2.126
- wasm-bindgen-futures 0.4.76
- wasm-bindgen-macro 0.2.126
- wasm-bindgen-macro-support 0.2.126
- wasm-bindgen-shared 0.2.126
- web-sys 0.3.103
- windows-link 0.2.1
- windows-sys 0.61.2

### MIT OR Apache-2.0 OR LGPL-2.1-or-later

- r-efi 5.3.0
- r-efi 6.0.0

### MIT OR Zlib OR Apache-2.0

- miniz_oxide 0.8.9

### MIT/Apache-2.0

- fallible-iterator 0.3.0
- fallible-streaming-iterator 0.1.9
- r2d2 0.8.10
- scheduled-thread-pool 0.2.7
- serde_urlencoded 0.7.1
- vcpkg 0.2.15
- version_check 0.9.5

### Unicode-3.0

- icu_collections 2.2.0
- icu_locale_core 2.2.0
- icu_normalizer 2.2.0
- icu_normalizer_data 2.2.0
- icu_properties 2.2.0
- icu_properties_data 2.2.0
- icu_provider 2.2.0
- litemap 0.8.2
- potential_utf 0.1.5
- tinystr 0.8.3
- writeable 0.6.3
- yoke 0.8.3
- yoke-derive 0.8.2
- zerofrom 0.1.8
- zerofrom-derive 0.1.7
- zerotrie 0.2.4
- zerovec 0.11.6
- zerovec-derive 0.11.3

### Unlicense OR MIT

- aho-corasick 1.1.4
- memchr 2.8.3

## Notices required for binary distribution

- **Apache-2.0 components** (notably `sync_wrapper`, which has no MIT
  alternative): Section 4 of the Apache License 2.0 requires that
  redistributions include a copy of the license and any NOTICE content.
- **BSD-3-Clause components** (`subtle`, and `matchit` in part) require their
  copyright notice and disclaimer to be reproduced in binary distributions.
- **Unicode-3.0 components** (`unicode-ident` and the `icu_*` crates) require
  their license text to accompany redistribution.

Full license texts are available in each crate's source, and in the Cargo
registry cache under `~/.cargo/registry/src/`.
