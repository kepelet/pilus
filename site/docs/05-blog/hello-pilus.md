---
title: Hello, pilus
type: blog
date: 2026-10-17
description: Why pilus is a terminal client.
---

Halo. Ini pilus.

Singkatnya, pilus adalah jendela untuk [Luvus](/blog/luvus). Luvus menjalankan coding agent saya di
latar belakang, dan pilus yang menampilkannya. Tutup jendelanya, agent tetap jalan. Buka lagi, semuanya
masih di tempat.

pilus bukan terminal emulator baru. Daemon Luvus mengirim layarnya sebagai grid sel, dan pilus
menggambarnya langsung, tanpa terminal di tengahnya.

## Isinya

- Workspace dan agent dari Mac sendiri dan mesin SSH, dalam satu daftar
- Diff dengan catatan review, riwayat Git, file explorer, dan port forwarder di panel kanan
- Browser kecil di sebelahnya, supaya `localhost:3000` tidak perlu keluar aplikasi
- Notifikasi saat agent menunggu jawaban atau selesai, bahkan ketika jendela sudah ditutup
- Aplikasi iOS untuk mengintip dan menjawab agent ketika saya afk
- [sipil](/sipil), agent yang bisa diajak ngobrol dan menyalakan coding agent atas nama saya

## Yang belum

- Hanya macOS di Apple Silicon. Linux masih TBD.
- Luvus harus sudah terpasang. pilus tidak membawanya.
- Masih alpha. Kalau ada yang rusak, ya rusak.

## Seberapa ringan?

Targetnya sekitar 60 MB RSS dan CPU yang tidur kalau saya tidak menyentuhnya. Kenapa saya begitu
peduli soal ini ada di [Lightweight](/features/lightweight). Intinya, mesin harian saya tidak sehebat
yang dulu, dan aplikasi yang makan baterai akan saya buang.

## Dari mana mulai?

[Instalasi](/installation), lalu [Memulai](/quickstart). Kalau penasaran kenapa Luvus dan bukan yang
lain, ceritanya ada di [sini](/blog/luvus).

Selamat mencoba. Kalau ketemu bug, saya ingin tahu.
