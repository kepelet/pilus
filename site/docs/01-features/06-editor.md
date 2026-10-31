---
title: Editor
description: Edit file teks dan markdown langsung di tab Files, lokal maupun lewat SSH.
sidebar:
  icon: file-pen
---

pilus punya editor teks sederhana. Belum bisa menyaingi vim, nano, Zed, atau Helix. Tapi saya menulis postingan blog [Memperkenalkan: pilus](/blog/alpha) di sini.

Editor ini pas untuk memperbaiki satu baris, menyunting `CLAUDE.md`, atau merapikan catatan tanpa pindah jendela. Apalagi kalau filenya ada di mesin remote.

## Membuka dan menyimpan

Klik file teks di tab [Files](/features/file-explorer), atau tekan `cmd+p` lalu ketik nama file. File teks mencakup markdown, file dengan bahasa yang dikenali, dan beberapa jenis teks biasa. pilus membaca dan menyimpan file sendiri, secara lokal maupun lewat SSH, tanpa melalui daemon.

- File markdown terbuka dalam salah satu dari tiga mode: **Source**, **Preview**, atau tampilan berdampingan.
- File lain langsung terbuka di source. Di luar mode edit, file hanya bisa dibaca, tetapi teksnya tetap bisa dipilih dan disalin.
- Tekan `cmd+s` untuk menyimpan. `cmd+r` memuat ulang file sambil mempertahankan mode, posisi caret, dan posisi scroll.
- `Esc`, `cmd+w`, atau tombol tutup di header mundur satu tingkat. Dari mode edit, file kembali ke read-only. Dari read-only, file ditutup.
- Kalau kamu menutup, memuat ulang, atau membuka file lain saat masih ada perubahan yang belum disimpan, pilus akan bertanya: **Save**, **Don't Save**, atau **Cancel**.
- Saat menyimpan lewat SSH, pilus menulis isi baru ke file sementara terlebih dahulu, lalu menyalinnya ke lokasi semula setelah semua byte tiba. Kalau transfer gagal, file asli tetap utuh. Mode file dan symlink juga dipertahankan.

Saat mulai mengedit, kolom kanan melebar agar pohon file dan dokumen muat berdampingan. Kolom kembali ke lebar semula setelah file ditutup. Tombol panel di header menyembunyikan atau menampilkan pohon file.

## Kalau file berubah di disk

pilus memantau file yang sedang terbuka. Jika agent mengubah file sebelum kamu mengeditnya, pilus memuat ulang isi file dan mempertahankan posisi caret. Kalau kamu sudah membuat perubahan, sebuah bar akan menawarkan dua pilihan: **Reload** membuang perubahanmu, sedangkan **Keep Mine** membiarkan penyimpanan berikutnya menimpa versi di disk.

File yang terlalu besar atau terbaca terpotong tidak bisa dibuka untuk diedit.

## Fitur editor

- Markdown mendapat syntax highlighting, termasuk front matter dan blok kode. MDX dan bahasa lain yang tata bahasanya tersedia juga disorot. TypeScript dan JSX dibaca sebagai JavaScript. TOML dan beberapa format lain belum mendapat warna.
- **Penanda perubahan Git** muncul di gutter. Batang hijau menandai baris baru atau berubah, sementara segitiga merah menunjukkan baris yang dihapus. Header menampilkan hitungan `+N -M`. Di preview markdown, penanda yang sama muncul pada tiap blok.
- Baris tempat caret berada tetap disorot, bahkan saat fokus ada di terminal. Jadi kamu bisa melihat tempat terakhir berhenti.
- Diagram `mermaid` di preview dirender sebagai gambar. Kalau ada kesalahan di sumbernya, diagram tetap tampil sebagai kode dan preview tidak menghilang.
- Gambar dibuka sebagai gambar. Untuk SVG, ikon pensil di header beralih ke source agar kamu bisa mengeditnya.

## Mengetik

Undo mencatat perubahan per kata, bukan menyimpan salinan seluruh file. Ketikan atau penghapusan yang berurutan digabung menjadi satu langkah undo. Langkah baru dimulai saat kamu mulai mengetik kata baru, memindahkan caret, atau setelah satu detik.

| Tombol | Aksi |
| --- | --- |
| `tab` / `shift+tab`, `cmd+]` / `cmd+[` | Indent/outdent |
| `cmd+/` | Beri atau lepas komentar |
| `cmd+l` | Pilih baris |
| `cmd+shift+k` | Hapus baris |
| `alt+up` / `alt+down` | Pindahkan baris |
| `alt+shift+up` / `alt+shift+down` | Gandakan baris |
| `cmd+enter` / `cmd+shift+enter` | Buat baris baru di bawah/di atas |
| `ctrl+g` | Ke baris (`12` atau `12:5`) |
| `PageUp` / `PageDown` | Gulir setinggi layar |

`Enter` mempertahankan indentasi baris. Di markdown, tombol ini melanjutkan item daftar, termasuk daftar bernomor dan checkbox kosong. Kalau item daftar sudah kosong, `Enter` mengakhirinya.

## Cari dan ganti

- `cmd+f` mencari di file yang sedang terbuka. Pencarian huruf kecil tidak membedakan huruf besar dan kecil. Begitu kamu mengetik huruf besar, pencarian mulai membedakannya. Perilaku ini disebut *smart case*. Di preview markdown, `cmd+f` lebih dulu beralih ke source.
- Tekan `cmd+alt+f` atau klik chevron di bar untuk membuka kolom ganti. `Enter` mengganti kecocokan yang dipilih lalu berpindah ke kecocokan berikutnya. `cmd+enter` mengganti semua kecocokan sekaligus sebagai satu edit, jadi satu kali undo mengembalikannya.
