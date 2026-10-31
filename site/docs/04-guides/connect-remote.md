---
title: Hubungkan ke mesin remote
type: guide
date: 2026-10-17
description: Tampilkan workspace dan agent dari mesin SSH di jendela pilus.
---

Setiap mesin yang diaktifkan menjadi satu endpoint: workspace-nya muncul di daftar yang sama dengan
Local, dan agent-nya di bagian Agents.

Kebanyakan pekerjaan saya berjalan di komputer lain, jadi bagian ini penting buat saya.

## Sebelum mulai

- Mesin harus bisa dijangkau melalui `ssh` tanpa prompt kata sandi (pilus memakai `BatchMode=yes`),
  misalnya dengan kunci SSH atau entri di `~/.ssh/config`. Host key-nya juga harus sudah dikenal:
  jalankan `ssh <host> true` sekali dari terminal kalau belum pernah.
- `luvus` terpasang di sana, atau biarkan pilus yang memasangkannya.
- Versi Luvus di sana harus cocok dengan yang di Mac kamu. Protokol tampilannya bisa berbeda antar versi.

## 1. Tambah mesin

Buka **Settings** (`cmd+shift+.`), cari bagian **REMOTE MACHINES**, dan pilih **Add Machine**:

| Kolom | Arti |
| --- | --- |
| Profile id | Nama pendek stabil seperti `mac-mini`. |
| SSH host or alias | Tujuan SSH: `user@host` atau alias di `~/.ssh/config`. |
| Label | Nama yang tampil. Opsional. |
| Remote session | Sesi Luvus yang dipakai di mesin tersebut. Opsional. |
| Install luvus if needed | Pasang `luvus` di mesin tujuan. |

Id dan host wajib diisi. Di balik layar pilus menjalankan `luvus machine add <id> --host <host>`, jadi
profil yang sama juga bisa dikelola dari terminal.

## 2. Tunggu terhubung

Mesin yang aktif disambungkan paralel saat pilus dibuka dan setiap kali katalog berubah. Titik
online/offline di daftar menunjukkan statusnya. Kalau terputus, pilus mencoba lagi sendiri.

Di daftar yang sama, tiap mesin punya tombol untuk menonaktifkan atau mengaktifkannya, memperbarui
Luvus-nya, merestart daemon di sana, dan menghapus profilnya.

Menghapus tidak menyentuh mesinnya, hanya profil di sini.

## 3. Buka workspace di remote machine

Jalankan `cmd+shift+o`, lalu pilih mesin di dropdown host. Folder dijelajahi lewat SSH. Tab, status bar, dan tema
mengikuti mesin yang sedang aktif. Folder yang sering dipakai bisa di-pin supaya satu klik dari
sidebar, bahkan ketika kamu sedang berada di mesin lain.

## 4. Menggunakan

- Klik baris workspace atau agent di mesin tersebut.
- Server dev di mesin tersebut bisa dijangkau lewat [Port forwarder](/features/port-forwarder).
- File di sana bisa dilihat dan disunting dari [tab Files](/features/file-explorer), dan screenshot
  yang kamu tempel masuk ke mesin tersebut, bukan ke local Mac kamu.
- Dari ponsel, satu perangkat yang dipasangkan bisa mengakses semua mesin ini; lihat [iOS](/features/ios).

## Kalau tidak jalan

- Coba `ssh <host> true` dari terminal. Kalau meminta kata sandi, pilus juga akan gagal.
- **Host key verification failed**: host key mesin itu belum ada di `known_hosts`. Sambungkan sekali
  dari terminal untuk menerimanya.
- **luvus binary not found on PATH**: pastikan `luvus` ada di `PATH` non-interaktif mesin tersebut, atau
  ulangi dengan mencentang *Install luvus if needed*.
- **protocol version mismatch** atau **display protocol N does not match local**: Luvus di mesin tersebut
  lebih lama atau lebih baru dari yang di Mac kamu. Gunakan **Update Luvus** pada mesin tersebut, lalu
  restart daemonnya.
- Daemon di sana baru memuat Luvus yang baru setelah di-restart. **Restart** dan **Stop & upgrade**
  menghentikan daemon lama, yang berarti pane di sesi tersebut akan tertutup, jadi pilus meminta
  konfirmasi dulu.
