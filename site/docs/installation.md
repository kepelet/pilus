---
title: Instalasi
description: Pasang pilus di Mac dan biarkan pilus yang mengatur Luvus.
sidebar:
  icon: download
---

## Persyaratan

- Koneksi internet.
- Mac dengan Apple Silicon (aarch64) dan macOS 13 atau lebih baru. Belum ada rilis untuk Intel atau GNU/Linux.
- Luvus. Kalau belum terpasang, pilus akan mengurusnya saat pertama kali dibuka. Lihat bagian [Luvus](#luvus).
- Opsional: [`gh`](https://cli.github.com) yang sudah login, untuk bagian Repositories di sidebar.
- Opsional: `ssh` yang bisa tersambung tanpa meminta kata sandi, kalau kamu ingin memakai [mesin remote](/guides/connect-remote).

## Pasang dari DMG

1. [Unduh](https://usepilus.dev/download) `pilus-v<versi>-macos-arm64.dmg`.
2. Buka DMG, lalu seret `pilus.app` ke folder `Applications`.
3. Buka pilus. Aplikasi ditandatangani dengan Developer ID dan sudah dinotarisasi Apple, jadi Gatekeeper seharusnya membukanya tanpa peringatan.

macOS mungkin meminta izin **Local Network**. Izin ini dipakai untuk menjangkau ponsel yang sudah dipasangkan saat keduanya berada di jaringan yang sama. Kalau kamu tidak memakai [aplikasi iOS](/features/ios), kamu bisa menolak izin itu.

## Luvus

pilus adalah klien. Sesi, workspace, dan agent dikelola oleh daemon Luvus. Saat pertama kali dibuka, pilus akan memasang Luvus kalau tidak menemukannya.

- Setiap build pilus memakai satu versi Luvus yang sudah diuji dan dipasangkan dengan checksum. pilus tidak otomatis memasang rilis Luvus terbaru karena protokolnya bisa berubah.
- Kalau kamu memasang Luvus lewat Homebrew, Cargo, atau Nix, pilus tidak akan menggantinya. Perbarui versi itu lewat package manager yang kamu gunakan.
- pilus menyalakan daemon kalau belum berjalan. Kamu tidak perlu menjalankan `luvus server start` sendiri.
- Buka **Settings -> About Luvus** untuk melihat versi yang terpasang.

## Pembaruan

Pilus bisa memperbarui dirinya sendiri, tetapi pemeriksaan otomatis **mati secara default**. Untuk memeriksa versi baru, pilih **Check for Updates** dari menu bar pilus. Kalau ingin pemeriksaan otomatis, nyalakan *Check for updates on launch* di **Settings -> About pilus**.

Di halaman yang sama, kamu bisa memilih channel:

| Channel | Isi |
| --- | --- |
| `stable` (default) | Rilis bertag |
| `preview` | Build nightly |

Sebelum dipasang, setiap unduhan harus memiliki tanda tangan minisign yang valid. pilus menolak pembaruan jika tanda tangannya tidak cocok.
