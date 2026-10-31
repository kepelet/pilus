---
title: File Explorer
description: File tree workspace di panel kanan, dengan tanda perubahan Git, unggah, dan unduh.
sidebar:
  icon: folder-tree
---

Tab **Files** menampilkan file tree workspace yang sama dengan TUI Luvus, ditambah beberapa cara mengelola file: unggah, unduh, ganti nama, dan hapus. Untuk memindahkan file ke komputer yang tidak bisa dijangkau dari pilus, saya biasanya masih memakai AirDrop atau Taildrop (btw).

## Memakainya

- Klik file teks untuk membukanya di [editor](/features/editor). Klik gambar (png, jpg, gif, webp, bmp, ico, tiff, atau svg) untuk melihatnya. File lain dibuka lewat daemon dalam kartu terminal. Klik 2x pada file apa pun juga membukanya lewat daemon.
- Klik folder untuk melipat atau membukanya. Keadaan lipatan disimpan per mesin, lalu dipulihkan saat pilus dibuka lagi.
- File yang berubah dan folder induknya diberi penanda. Klik file yang berubah untuk membuka diff-nya.
- Klik kanan untuk membuat, mengganti nama, atau menghapus entri.
- Tekan `cmd+p` untuk membuka *quick open*. Ketik sebagian nama file untuk mencarinya di seluruh workspace. Daftarnya dibuat dengan `git ls-files` di repo Git, atau `find` di luar repo.

Fitur ini bekerja sama di mesin lokal maupun SSH. Sebagian operasi lewat Luvus UHP. Untuk hal yang belum atau tidak didukung UHP, seperti membaca dan menulis isi file, pilus memakai SSH langsung.

## Markdown

pilus membaca file markdown dan menampilkannya sebagai dokumen, termasuk diagram `mermaid`. Pilih teks untuk menyalinnya atau mengirim kutipan ke agent yang sedang berjalan. Kamu juga bisa menyimpan pilihan sebagai **catatan**, menambahkan permintaan, lalu mengirim beberapa catatan sekaligus dalam satu prompt.

Catatan disimpan per mesin dan file di `pilus.db`, bukan sebagai review note daemon. File yang tidak berubah tidak punya diff untuk ditempeli catatan.

Untuk menyunting file, lihat [Editor](/features/editor).

## Unggah dan unduh

Seret file atau folder ke tab Files untuk mengunggahnya. pilus memberi tahu lokasi file setelah selesai. Untuk mengunduh, klik kanan file. Di mesin remote, ini bisa menggantikan `scp`.

Untuk mengirim gambar atau screenshot ke agent, lihat [Terminal](/features/terminal#tempel-gambar). Tempel dengan `cmd+v` dan pilus akan mengurus lokasi file-nya.
