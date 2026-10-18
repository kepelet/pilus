---
title: Instalasi
description: Pasang pilus di Mac, siapkan Luvus, dan hubungkan keduanya.
sidebar:
  icon: download
---

## Persyaratan

- Mac dengan Apple Silicon (aarch64)
- CLI `luvus` ada di `PATH`. pilus menjalankan daemon Luvus sendiri kalau belum berjalan, jadi
  kamu tidak perlu menyalakannya manual.
- Opsional: [`gh`](https://cli.github.com) yang sudah login, untuk bagian Repositories di sidebar.

## Pasang dari DMG

1. Unduh `pilus-v<versi>-macos-arm64.dmg`. <!-- TODO: link unduhan resmi -->
2. Buka DMG, lalu seret `pilus.app` ke `Applications`.
3. Buka pilus. Build rilis sudah ditandatangani dan dinotarisasi, jadi Gatekeeper tidak akan protes.

Setelah itu pilus memperbarui dirinya sendiri; lihat [Pembaruan](#pembaruan).

## Build dari source

Butuh Rust 1.96+. Di macOS, command-line tools saja biasanya cukup: shader Metal sudah
di-stitch dan build tidak memanggil compiler `metal`.

```sh
git clone git@github.com:kepelet/pilus.git
cd pilus

cargo run -p pilus                 # sesi default
cargo run -p pilus -- --session work   # sesi bernama
just bundle                        # target/release/pilus.app
```

Untuk memeriksa koneksi tanpa membuka jendela:

```sh
cargo run -p pilus --no-default-features -- --probe
```

## Pembaruan

`pilus → Check for Updates…` memeriksa sekarang juga. Lencana di footer menunjukkan unduhan dan
tombol restart. Di **Settings → UPDATES** kamu bisa memilih channel:

| Channel | Isi |
| --- | --- |
| `stable` (default) | Rilis bertag. |
| `preview` | Build nightly. Alias: `beta`, `nightly`, `canary`. |

Setiap unduhan harus memiliki tanda tangan minisign yang valid sebelum dipasang.

## Langkah berikutnya

Lanjut ke [Memulai](/quickstart).
