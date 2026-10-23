---
title: Why a native client
type: blog
date: 2026-10-18
description: Kenapa permukaan Luvus saya pindahkan dari terminal ke jendela native.
---

Pertanyaan yang paling masuk akal soal pilus: Luvus kan sudah punya antarmuka di terminal, kenapa
dibuatkan jendela sendiri?

Jawaban jujurnya, antarmuka terminal itu bagus sampai kamu ingin sesuatu yang bukan teks.

## Apa yang tidak muat di grid

Luvus menggambar semuanya sebagai grid sel bergaya. Itu cocok untuk pane dan tab, tapi canggung untuk:

- diff yang ingin kamu beri catatan per baris, lalu kirim ke agent
- halaman web dari dev server, tanpa pindah ke Chrome
- notifikasi yang berbunyi ketika agent berhenti menunggu kamu, padahal terminalnya sedang di balik
  aplikasi lain
- menyeret folder ke jendela untuk mengunggahnya ke mesin remote

Semua itu bisa dipaksakan ke dalam sel, tapi hasilnya hampir selalu aneh.

## Jadi, emulator baru?

Bukan. Daemon mengirim layarnya sebagai grid sel bergaya, dan pilus menggambarnya langsung dengan GPUI
di atas Metal. Tidak ada emulator terminal di tengah. Sidebar, tab, status bar, dan panel kanan digambar
sebagai elemen native, di atas grid yang sama.

Daemon tetap pemilik layout dan tema. pilus hanya memutuskan bagaimana baris yang ia klaim digambar.

## Yang tidak bisa diambil alih

Ini bagian yang kurang keren. Dari Luvus, hanya sidebar workspace dan pill nama sesi yang bisa
diklaim klien. Tab bar dan status line tetap digambar daemon, dan tidak ada pesan untuk mengambilnya.
Jadi pilus menutupinya dengan versi native. Barisnya tetap dicadangkan di bawahnya.

Itu bukan kompromi yang saya bangga-banggakan, tapi itu yang bisa dilakukan hari ini dengan API yang
ada.

## Biayanya

Antarmuka native yang boros tidak ada gunanya buat saya. Itu sebabnya pilus menjaga timer, polling, dan
animasinya tetap sedikit, dan berhenti total ketika tidak ada yang berubah. Detailnya ada di
[Lightweight](/features/lightweight).

## Apa yang sebenarnya didapat

Menutup jendela tidak mematikan apa pun, karena yang menjalankan agent adalah Luvus. pilus tinggal
terhubung lagi, termasuk ke mesin SSH yang sempat putus. Dari sini, aplikasi iOS dan
[sipil](/sipil) hanyalah klien lain dari daemon yang sama.

Dan tanpa Luvus, pilus tidak akan pernah ada.
