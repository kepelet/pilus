---
title: In-app Browser
description: Buka halaman di panel kanan tanpa meninggalkan pilus.
sidebar:
  icon: globe
---

Browser ini memakai webview sistem (WebKit di macOS, melalui `wry`) di kolom kanan, tepat di bawah address bar. Buka lewat tombol Browser di header panel kanan. Kalau panelnya tersembunyi, tekan `cmd+b`.

Saya paling sering memakainya untuk membuka YouTube sambil menunggu agent. Tapi browser ini juga berguna untuk melihat `localhost:3000` tanpa pindah jendela.

## Memakainya

- Ketik alamat di address bar. Kalau yang kamu ketik bukan alamat, pilus mencarinya lewat DuckDuckGo.
- Alamat tanpa skema dibuka lewat `https`. Pengecualian: `localhost`, `*.localhost`, dan alamat IPv4 dibuka lewat `http`, karena server development biasanya tidak memakai TLS.
- Saat kamu tidak sedang mengeditnya, address bar mengikuti halaman yang sedang dibuka.
- Kamu bisa membuka sampai delapan tab. Setiap tab punya webview sendiri dan terus berjalan meski sedang tidak terlihat, seperti tab latar di browser biasa.
- Halaman yang meminta jendela baru dibuka di tab yang sama.
- Webview dibuat saat alamat pertama kali dimuat dan tetap aktif saat kamu berpindah tab, jadi halaman tidak perlu dimuat ulang. pilus menyembunyikannya saat elemen lain, seperti menu, perlu tampil di atasnya.

## Unduhan

pilus meminta konfirmasi sebelum mengunduh file. Kalau unduhan tidak punya nama, pilus memberinya nama.

## Dev server remote

Dev server di mesin SSH biasanya hanya mendengarkan loopback mesin itu, jadi browser di Mac tidak bisa menjangkaunya langsung. Teruskan port-nya dengan [Port forwarder](/features/port-forwarder), lalu buka `localhost:<port>` di sini.

## Batasan

Browser ini masih sederhana. Situs internet yang hanya melayani HTTP biasa (bukan HTTPS) belum bisa dibuka. Alamat lokal seperti `localhost` dan IP tetap bisa. Setiap tab memakai satu proses WebContent penuh, jadi jumlah tab dibatasi. Untuk kebutuhan lain, browser biasa masih lebih cocok.
