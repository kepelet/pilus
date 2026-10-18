---
title: Port Forwarder
description: Lihat port yang didengarkan mesin remote dan jangkau dari Mac.
sidebar:
  icon: network
---

`npm run dev` di Mac mini mendengarkan loopback Mac mini, di luar jangkauan browser di sini. Tab
**Ports** di panel kanan menampilkan apa yang didengarkan mesin aktif dan menjembataninya.

## Cara kerja

- Daftar port dibaca dari mesin itu sendiri lewat profil SSH tersimpan, dengan `lsof` atau `ss`.
  Tidak ada yang perlu dipasang di host selain salah satunya.
- Hanya port milik akunmu yang ditampilkan, jadi `sshd` dan resolver tidak ikut.
- **Forward** menjalankan `ssh -N -L` sehingga `localhost:<port>` di Mac mencapai port itu.
- Forward dicatat per mesin, jadi tetap berjalan saat kamu pindah ke mesin lain. **Stop** atau
  menutup jendela mengakhirinya.
- Mac sendiri tidak perlu forward: port-nya sudah di `localhost`, jadi barisnya hanya bisa dibuka.

Daftar hanya dibaca selagi tab Ports terlihat.
