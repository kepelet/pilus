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
- Alamat tanpa skema dibuka lewat `https`. Pengecualian: `localhost`, `*.localhost`, dan alamat IPv4 dibuka lewat `http`, karena server development biasanya tidak memakai TLS. Alamat yang kamu tulis lengkap dengan `http://` selalu dihormati, tetapi macOS hanya memuat situs http di luar daftar itu kalau namanya diizinkan saat build (lihat [Batasan](#batasan)).
- Saat kamu tidak sedang mengeditnya, address bar mengikuti halaman yang sedang dibuka.
- Kamu bisa membuka sampai delapan tab. Setiap tab punya webview sendiri dan terus berjalan meski sedang tidak terlihat, seperti tab latar di browser biasa.
- Halaman yang meminta jendela baru dibuka di tab yang sama.
- Webview dibuat saat alamat pertama kali dimuat dan tetap aktif saat kamu berpindah tab, jadi halaman tidak perlu dimuat ulang. pilus menyembunyikannya saat elemen lain, seperti menu, perlu tampil di atasnya.

## Menu tiga titik

Tombol tiga titik setelah address bar membuka menu native:

- **Notes** menyalakan penanda catatan. Arahkan pointer ke elemen untuk melihat garis luarnya, klik, lalu tulis catatannya di kolom yang digambar pilus. Catatan tampil sebagai lencana bernomor di elemen itu dan tercatat di tray. **Copy notes** menyalin semuanya sebagai teks, siap ditempel ke agent. Catatan menempel pada tab-nya.
- **Viewport** membentuk halaman di kartu: Responsive (mengisi kolom), 16:10 (1280×800), 4:3 (1024×768), Mobile, atau Tablet. Kartu mengecil agar muat di kolom, tetapi halaman ditata pada ukuran aslinya, jadi media query cocok. Pilihan berlaku per tab.
- **Open in Default Browser** membuka alamat tab ini di browser bawaan Mac.
- **Copy URL** menyalin alamatnya.

## Dipakai agent

Agent yang berjalan di pane bisa membuka dan menjalankan halaman lewat `pilus ctl browser …`, misalnya untuk memeriksa `localhost:3000` yang baru ia ubah. Kamu tetap pemegang kendali:

- Tab yang dibuka agent miliknya, bergaris putus-putus dan diberi chip di strip tab. Tab itu incognito, jadi tidak membawa login-mu, dan agent hanya memakai tab seperti itu.
- Tab milikmu tertutup untuk agent. Ia hanya bisa meminta lewat `browser request`, dan kamu memilih **Allow** atau **Deny**. Allow meminjamkan tab sampai dilepas, diambil kembali, atau ditutup.
- Selama agent bekerja, tab itu terkunci. Penutup transparan menahan klik, scroll, dan ketikanmu, halaman diberi bingkai beraksen, dan tombol **Take over** di pojok kanan atas mengakhiri kunci.
- Agent bisa membaca (`snapshot`, `text`, `network`, `console`), menjalankan (`click`, `fill`, `press`, `scroll`, `drag`), dan mengatur viewport. Kursor virtual menunjukkan ke mana ia menuju.
- Agent di mesin SSH bisa memakainya juga bila kamu mengizinkannya di tab [Ports](/features/port-forwarder#agent-di-mesin-remote).

## Unduhan

pilus meminta konfirmasi sebelum mengunduh file. Kalau unduhan tidak punya nama, pilus memberinya nama.

## Dev server remote

Dev server di mesin SSH biasanya hanya mendengarkan loopback mesin itu, jadi browser di Mac tidak bisa menjangkaunya langsung. Teruskan port-nya dengan [Port forwarder](/features/port-forwarder), lalu buka `localhost:<port>` di sini.

## Batasan

Browser ini masih sederhana. Situs internet yang hanya melayani HTTP biasa (bukan HTTPS) belum bisa dibuka. Alamat lokal seperti `localhost` dan IP tetap bisa. Untuk nama di jaringan privat, seperti `*.lab`, jalankan `just bundle` dengan `PILUS_HTTP_HOSTS=lab` (pisahkan beberapa nama dengan koma). Daftar ini masuk ke Info.plist saat build karena aplikasi yang sudah ditandatangani tidak bisa mengubahnya, dan kosong secara default. Setiap tab memakai satu proses WebContent penuh, jadi jumlah tab dibatasi. Untuk kebutuhan lain, browser biasa masih lebih cocok.
