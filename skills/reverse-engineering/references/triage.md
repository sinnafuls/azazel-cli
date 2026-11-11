# Triage

Goal: in a few minutes, know what the artefact is and where to start. Record every result in the findings file.

## 1. Identity

```sh
sha256sum target.bin          # Windows: certutil -hashfile target.bin SHA256
file target.bin               # container, architecture, linkage
ls -l target.bin              # size
xxd target.bin | head -4      # first bytes
```

Common first bytes:

| Bytes | Meaning |
|---|---|
| `4D 5A` (`MZ`) | DOS/PE executable or DLL |
| `7F 45 4C 46` | ELF |
| `CF FA ED FE`, `CE FA ED FE`, `CA FE BA BE` | Mach-O (64-bit, 32-bit, universal) |
| `50 4B 03 04` | ZIP container: also JAR, APK, DOCX, many game archives |
| `1F 8B` | gzip |
| `28 B5 2F FD` | Zstandard |
| `89 50 4E 47` | PNG |
| `25 50 44 46` | PDF |
| `53 51 4C 69 74 65` | SQLite database |
| `00 61 73 6D` | WebAssembly |
| `64 65 78 0A` | Android DEX |

An archive or installer is unpacked first and each member triaged on its own.

## 2. Headers and sections

| Format | Commands |
|---|---|
| PE | `dumpbin /headers /imports /exports`, or `objdump -x`, or a PE viewer |
| ELF | `readelf -hSl`, `readelf -d`, `readelf --syms`, `objdump -x` |
| Mach-O | `otool -hlL`, `nm -m`, `codesign -dvv` |

Note: architecture and bitness, image base, entry point, subsystem, timestamps, whether it is position independent, whether it is stripped, and any debug-file reference (PDB path, `.gnu_debuglink`, dSYM UUID). A debug path often names the project and its source layout.

## 3. Strings

```sh
strings -a -n 6 target.bin | less
strings -a -el -n 6 target.bin   # UTF-16LE, common on Windows
```

Look for: version and build strings, compiler banners, file paths from the build machine, format strings, error messages, URLs and host names, registry keys, command-line options, names of libraries and their versions, class and method names (Objective-C, .NET, Java, Go and Rust binaries carry many).

Few readable strings in a large file suggests packing or encryption.

## 4. Imports and exports

Imports say what the program can do; group them:

| Group | Examples | Suggests |
|---|---|---|
| Files | `CreateFile`, `fopen`, `mmap` | Parsers, configuration, save data |
| Network | `connect`, `WSASend`, `curl_easy_perform`, TLS libraries | A protocol to analyse |
| Crypto | `BCrypt*`, `EVP_*`, `CCCrypt` | Where keys and algorithms are chosen |
| Compression | `inflate`, `ZSTD_decompress`, `LZ4_*` | Container formats |
| Process and memory | `VirtualAlloc` with execute rights, `dlopen`, `ptrace` | Loaders, plugins, self-modification |
| Scripting | Lua, Python, V8, Mono symbols | Logic may live in scripts, not native code |

A very short import list on a large binary (only a loader function or two) is another packing indicator.

## 5. Toolchain and runtime

Knowing the language changes how you read the code.

| Indicator | Runtime |
|---|---|
| `mscoree.dll` import, CLR header | .NET: decompile to C# instead of disassembling |
| `classes.dex`, `META-INF` | Android or Java: use a Java decompiler |
| `go.buildid`, `runtime.` symbols, `.gopclntab` | Go: function names survive stripping in the pclntab |
| `rust_panic`, mangled `_ZN…17h…E` names, panic paths with `.rs` | Rust |
| `PyInstaller`, `_MEIPASS`, embedded `.pyc` | Packaged Python: extract the archive |
| Electron or NW.js files, `app.asar` | JavaScript in an archive |
| Unity `globalgamemanagers`, `il2cpp`, `UnityPlayer` | Unity: managed assemblies or IL2CPP metadata |
| `.pak`, `UE4`/`UE5` strings | Unreal Engine: assets in pak or IoStore containers |

## 6. Packing and obfuscation indicators

- Section names that are not the toolchain's usual ones (`UPX0`, random names), or a section that is writable and executable.
- High entropy across the code section. Estimate with a short script: byte histogram, Shannon entropy per 4 KiB block. Values near 8 bits per byte mean compressed or encrypted data.
- Entry point outside the first code section.
- Raw size far smaller than virtual size for a section.
- Almost no imports and almost no strings.

If packed: identify the packer from section names and strings; a known packer has a known unpacker. Do not run the sample to unpack it unless the user has provided an isolated environment for that purpose. Report the finding and the options.

## 7. Triage summary

End with a paragraph in the findings file:

> `target.bin` (SHA-256 …, 4.2 MB) is a 64-bit PE built with MSVC 19.3x, not packed, stripped of symbols but with a PDB path naming project `foo`. It links statically against zlib 1.2.13 and OpenSSL 3. Network and file imports are present. Strings name a configuration file `settings.dat` and an endpoint path `/api/v2/sync`. Start from the references to those two strings.
