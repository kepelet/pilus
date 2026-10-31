---
title: Terminal
description: Menyalin, membuka link dan path, menempel gambar, dan bertahan di koneksi yang lambat.
sidebar:
  icon: terminal
---

## Tab dan pane

| Tombol | Aksi |
| --- | --- |
| `cmd+t` | Buka tab baru di workspace aktif |
| `cmd+shift+t` | Buka terminal lokal |
| `cmd+d` / `cmd+shift+d` | Split vertikal / horizontal |
| `cmd+1` sampai `cmd+9` | Pilih tab ke-N |
| `cmd+w` | Tutup tab |

Klik kanan tab untuk mengganti nama atau menutupnya. Kalau belum diberi nama, tab menampilkan proses yang sedang berjalan di depannya, atau folder kerja jika tidak ada proses. Kamu bisa memperbesar pane yang sedang difokuskan lewat toolbar-nya. Aktifkan toolbar dari command palette. Seret garis pemisah antar-pane untuk mengubah ukurannya.

## Menyalin dan menempel

Pilih teks dengan pointer, lalu tekan `cmd+c` untuk menyalin. `cmd+a` memilih seluruh layar. Seleksi tidak melewati batas pane. Tekan `cmd+v` untuk menempel. Kalau clipboard berisi gambar, lihat bagian [Tempel gambar](#tempel-gambar).

## Membuka link dan path

Tekan `cmd` sambil mengeklik teks di layar:

- **Link** yang dibuat program lewat OSC 8 dibuka di browser sistem jika skemanya `http`, `https`, atau `mailto`. Skema lain diteruskan ke daemon.
- Jika program di pane meminta pilus membuka URL, pilus hanya mengikuti permintaan untuk skema `http`, `https`, dan `mailto`. Ini disengaja. Permintaan bisa berasal dari apa pun yang berjalan di pane, termasuk mesin remote, dan skema seperti `file:` bisa membuka aplikasi. Jadi link `file://` dari daemon tidak dibuka.
- **Path file**, seperti `src/main.rs:42:7` atau `"notes.md".`, diperiksa dulu di mesin yang menjalankan pane. Untuk mesin remote, pilus memeriksanya lewat SSH. Kalau file ditemukan, pilus membukanya di jendela ini lewat [editor](/features/editor) atau sebagai gambar. Panel kanan juga dibuka jika sebelumnya tersembunyi. Kalau tidak ditemukan, klik diteruskan ke daemon.

Pengecekan menahan klik sebentar, tetapi tidak membuat jendela macet.

## Tempel gambar

Agent bisa membaca gambar dari path, tetapi pane tidak menerima piksel secara langsung. Jadi, saat kamu menekan `cmd+v` atau menyeret gambar ke terminal, pilus menempelkan **path** gambar ke pane aktif.

| Sumber | Pane lokal | Pane remote |
| --- | --- | --- |
| File gambar (seret, atau salin dari Finder) | Path file itu sendiri, tanpa salinan | Dikirim lewat SSH ke `~/.pilus/paste/` |
| Piksel di clipboard (misalnya screenshot) | Ditulis ke `~/.pilus/paste/` | Dikirim lewat SSH ke `~/.pilus/paste/` |

Beberapa hal yang perlu diketahui:

- File disimpan di mesin tempat pane berjalan, sama seperti unggahan lewat tab Files. Path yang ditempel menunjuk ke file di mesin itu.
- Folder dan file hanya bisa dibaca oleh pemiliknya.
- File tempelan dihapus setelah tujuh hari, saat ada tempelan berikutnya. pilus tidak menimpa file lama (kalo nimpa dipikir mas 😹)
- Gambar yang lebih besar dari 32 MB ditolak.
- Clipboard berisi teks akan ditempel sebagai teks biasa. Gambar hanya ditempel sebagai gambar jika clipboard tidak berisi teks, atau jika sumbernya file gambar dari Finder.
- Saat menyeret beberapa file, pilus mengambil gambar saja dan melewati file lain dengan toast.

Dengan begitu, kamu bisa mengirim gambar ke coding agent di mesin remote tanpa menjalankan `scp` sendiri.

## Koneksi lambat

Untuk koneksi remote yang lambat, pilus memakai beberapa trik yang juga dipakai mosh:

- **Local echo**: pilus memperkirakan posisi huruf yang kamu ketik dan menggambarnya sebelum daemon membalas. Fitur ini aktif saat waktu balas terukur lebih dari 50 ms (ditandai lebih jelas di atas 100 ms). Tebakan yang tidak dikonfirmasi akan disembunyikan.
- SSH memakai kompresi karena frame penuh banyak mengulang data yang sama.
- pilus menunggu sampai ukuran jendela berhenti berubah sebelum mengirim ukurannya ke mesin remote. Jadi, menyeret tepi jendela tidak mengantre puluhan operasi reflow.
- pilus menyelesaikan pesan yang sedang diterima sebelum menggambar layar. Perintah seperti `clear` tidak akan muncul sesaat sebelum isi layar berikutnya.

## Yang masih bisa bermasalah

Masalah terbesar ada pada rendering: warna, Unicode dan glyph, serta kedipan atau redraw layar. Scrollback bisa dipakai dengan scroll mouse, dan salin-tempel seharusnya bekerja. pilus sudah menangani beberapa masalah rendering, tetapi sebagian lainnya mungkin perlu diperbaiki di Luvus.

Kalau kamu menemukan masalah, [buat issue](https://github.com/kepelet/pilus/issues).
