---
title: Hubungkan ke mesin remote
type: guide
date: 2026-10-17
description: Tampilkan workspace dan agent dari mesin SSH di jendela pilus.
---

Setiap mesin yang diaktifkan menjadi satu endpoint: workspace-nya muncul di daftar yang sama dengan
Local, dan agent-nya di dock AGENTS.

## Sebelum mulai

- Mesin itu bisa dijangkau lewat `ssh` tanpa prompt kata sandi (pilus memakai `BatchMode=yes`),
  misalnya dengan kunci SSH atau entri di `~/.ssh/config`.
- `luvus` terpasang di sana, atau biarkan pilus yang memasangkannya.

## 1. Tambah mesin

Buka jendela **Remote Machines** (tombol monitor di samping ikon Settings) dan pilih **Add Machine**:

| Kolom | Arti |
| --- | --- |
| ID | Nama pendek stabil, mis. `mac-mini`. |
| Host | Tujuan SSH: `user@host` atau alias di `~/.ssh/config`. |
| Label | Nama yang tampil. Opsional. |
| Session | Sesi Luvus yang dipakai di mesin itu. Opsional. |
| Install | Pasang `luvus` di mesin tujuan. |

Di balik layar pilus menjalankan `luvus machine add <id> --host <host>`, jadi profil yang sama
juga bisa dikelola dari terminal.

## 2. Tunggu terhubung

Mesin yang aktif disambungkan paralel saat pilus dibuka dan setiap kali katalog berubah. Titik
online/offline di daftar menunjukkan statusnya. Kalau terputus, pilus mencoba lagi sendiri.

## 3. Buka workspace di sana

`⌘⇧O`, lalu pilih mesin di dropdown host. Folder dijelajahi lewat SSH. Tab, status bar, dan tema
mengikuti mesin yang sedang aktif.

## 4. Pakai

- Klik baris workspace atau agent di mesin itu untuk berpindah. Layar mesin lain tidak mengalir
  selagi tidak aktif, tetapi daftarnya tetap hidup.
- Server dev di mesin itu bisa dijangkau lewat [Port forwarder](/features/port-forwarder).
- Dari ponsel, satu perangkat yang dipasangkan bisa mengakses semua mesin ini; lihat [iOS](/features/ios).

## Kalau tidak jalan

- Coba `ssh <host> true` dari terminal. Kalau meminta kata sandi, pilus juga akan gagal.
- Pastikan `luvus` ada di `PATH` non-interaktif mesin itu.
