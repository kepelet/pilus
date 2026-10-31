---
title: Port Forwarder
description: Lihat port yang didengarkan mesin remote dan jangkau dari Mac.
sidebar:
  icon: network
---

Saat kamu menjalankan `npm run dev` di Mac mini, server biasanya hanya menerima koneksi dari Mac mini itu sendiri. Browser di Mac kamu tidak bisa menjangkaunya langsung. Tab **Ports** di panel kanan menampilkan port yang sedang didengarkan mesin aktif dan bisa meneruskannya ke Mac.

## Cara kerja

- pilus membaca daftar port langsung dari mesin lewat profil SSH yang tersimpan. Ia memakai `lsof` atau `ss`; host hanya perlu punya salah satunya.
- Daftar hanya menampilkan port milik akunmu. `sshd` dan resolver tidak ikut ditampilkan.
- **Forward** menjalankan `ssh -N -L`, sehingga `localhost:<port>` di Mac terhubung ke port di mesin remote.
- Forward disimpan per mesin, jadi tetap berjalan saat kamu berpindah mesin. Tekan **Stop** atau tutup jendela untuk menghentikannya.
- Port di Mac lokal tidak perlu di-forward. Port itu sudah tersedia di `localhost`, jadi kamu bisa langsung membukanya.

pilus membaca daftar port hanya saat tab Ports terlihat.
