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

## Agent di mesin remote

`pilus ctl` mengendalikan jendela pilus dari skrip atau agent, termasuk `pilus ctl ports list` dan `pilus ctl ports forward <port>`. Perintah forward hanya menerima port yang sedang didengarkan mesin itu, sama seperti tombol Forward, dan menjawab dengan URL lokal yang bisa dibuka di [Browser](/features/in-app-browser).

Socket jendela tidak menyeberang antarmesin, jadi agent di Mac mini tidak menemukannya. Untuk mesin SSH yang sedang aktif, centang **Let agents on `<machine>` drive this window**. pilus lalu menahan `ssh -R` yang menaruh socket di `~/.pilus/pilus-remote.sock` pada mesin itu dan menyambungkannya lagi bila koneksi putus. Status `On`, `Connecting…`, atau alasan kegagalannya tampil di tab Ports.

- Mati untuk semua mesin sampai kamu mencentangnya. Mencentang berarti proses apa pun milik akunmu di mesin itu bisa memakai `pilus ctl`.
- Mesin itu butuh binary `pilus`, hanya untuk klien `ctl`. pilus tidak memasang apa pun di mesin remote.
