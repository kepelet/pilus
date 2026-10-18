---
title: iOS app
description: Pantau dan arahkan agent dari iPhone atau iPad.
sidebar:
  icon: smartphone
---

Aplikasi pendamping yang mengontrol mesinmu dari ponsel. Tersedia lewat TestFlight di iOS 17+.
<!-- TODO: link TestFlight -->

## Memasangkan

1. Di pilus: **Settings → Remote Access → Enable**.
2. **Add Device** menampilkan QR (tautan `pilus://pair/…`).
3. Di ponsel, pindai QR-nya.

Kode pairing hanya berlaku sekali dan sepuluh menit, dan hanya tampil di host. Setelahnya ponsel
dikenali dari identitas iroh-nya, jadi kamu tidak perlu memasangkan ulang.

## Koneksi

Ponsel berbicara ke host lewat QUIC peer-to-peer (iroh), tanpa port yang dibuka. Remote Access
berjalan sebagai service latar (LaunchAgent), jadi ia tetap ada saat jendela pilus ditutup dan
menyala lagi saat login. Satu pintu masuk melayani mesin ini dan semua mesin SSH-nya.

## Yang bisa dilakukan

Daftar agent dan workspace, layar agent, review diff (tambah dan kirim catatan ke agent), riwayat
Git, GitHub, Mission, tugas baru, dan notifikasi saat agent butuh kamu.

## Level akses

Tiap perangkat punya level yang bisa diturunkan kapan saja dari Settings, dan dicabut:

| Level | Boleh |
| --- | --- |
| View | Membaca daftar, status, dan snapshot. |
| Control | Mengirim prompt, interrupt, fokus, dan review diff. |
| Operate | Control, ditambah memulai tugas, agent, dan workspace. |

Operate setara dengan duduk di mesin itu, jadi hanya bisa diberikan lewat kode pairing baru dengan
**Allow this device to start work** dicentang.
