---
title: Memulai
description: Dari pilus yang baru dipasang sampai agent pertama yang berjalan.
sidebar:
  icon: rocket
---

## 1. Buka pilus

Saat pertama kali dibuka, pilus mencari socket klien Luvus. Kalau Luvus belum terpasang, pilus memasangnya. Kalau daemon belum berjalan, pilus menyalakannya. pilus juga menyesuaikan beberapa konfigurasi daemon agar tampilannya cocok dengan aplikasi.

## 2. Buka workspace

Tekan `cmd+shift+o` untuk memilih folder. Dropdown host di atas dialog menentukan mesin yang kamu jelajahi: Mac lokal atau mesin SSH. Kalau folder berada di repo Git, **Open with new worktree** akan membuat worktree dengan nama branch otomatis.

Folder yang sering kamu buka bisa di-pin lewat klik kanan. Setelah itu, folder muncul di menu pin pada header sidebar, dan `cmd+o` membukanya lewat palette.

## 3. Jalankan agent

Buka terminal, jalankan agent pilihanmu, lalu kirim prompt. Agent yang berjalan muncul di bagian **AGENTS** di sidebar, dikelompokkan berdasarkan mesin.

Pilih agent untuk memfokuskan pane-nya. Titik pada tab menunjukkan statusnya:

- Merah: agent menunggu konfirmasi atau jawabanmu.
- Oranye: agent sedang bekerja.
- Hijau: giliran agent selesai.
- Warna netral: agent sedang idle.

## 4. Tinggalkan pilus

Kamu bisa menutup jendelanya kapan saja. Luvus dan agent tetap berjalan. pilus memberi notifikasi dengan suara dan lencana saat agent menunggu jawabanmu atau menyelesaikan gilirannya. Buka pilus lagi dan workspace-mu masih ada.

## 5. Tinjau hasilnya

Tekan `cmd+b` untuk membuka panel kanan. Tab **Diff** menampilkan perubahan agent. Tambahkan catatan pada baris yang perlu diperbaiki, lalu kirimkan ke agent. Penjelasan lengkapnya ada di [Git](/features/git).

## Pintasan

| Tombol | Aksi |
| --- | --- |
| `cmd+shift+p` | Buka command palette |
| `cmd+shift+o` | Buka workspace |
| `cmd+o` | Buka workspace yang di-pin |
| `cmd+shift+s` | Pindah sesi Luvus |
| `cmd+t` | Buka tab baru |
| `cmd+d` / `cmd+shift+d` | Split vertikal / horizontal |
| `cmd+b` | Tampilkan/sembunyikan panel kanan |
| `cmd+s` | Tampilkan/sembunyikan sidebar kiri (kecuali saat mengedit file) |
| `cmd+p` | Quick open file |
| `cmd+f` / `cmd+alt+f` | Cari/ganti di file yang terbuka |
| `cmd+1` sampai `cmd+9` | Pilih tab ke-N |
| `cmd+plus` / `cmd+minus` / `cmd+0` | Perbesar/perkecil/reset ukuran font |
| `cmd+,` | Buka Appearances |
| `cmd+shift+.` | Buka Settings |

## Selanjutnya

- [Hubungkan ke mesin remote](/guides/connect-remote)
- [Antarmuka](/features/ui)
- [Editor](/features/editor)
- [Aplikasi iOS](/features/ios)
