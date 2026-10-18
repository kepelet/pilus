---
title: In-app Browser
description: Buka halaman di panel kanan tanpa meninggalkan pilus.
sidebar:
  icon: globe
---

Browser adalah webview sistem (WebKit di macOS, lewat `wry`) yang ditaruh di kolom kanan, di bawah
address bar.

## Memakainya

- Ketik alamat di address bar. Address bar mengikuti halaman saat tidak sedang kamu edit.
- Halaman yang meminta jendela baru dibuka di tempat yang sama.
- Webview dibuat saat alamat pertama dimuat dan tetap hidup selagi kamu pindah tab, jadi halaman
  tidak dimuat ulang. Ia disembunyikan saat ada elemen pilus yang akan menimpanya, misalnya menu.

## Unduhan

Setiap unduhan meminta konfirmasi dulu, dan unduhan tanpa nama diberi nama.

## Dev server remote

Server dev di mesin SSH hanya mendengarkan loopback mesin itu. Teruskan port-nya dengan
[Port forwarder](/features/port-forwarder), lalu buka `localhost:<port>` di sini.
