---
title: Lightweight
description: pilus menjadikan keringanan sebagai fitur utama
sidebar:
  icon: feather
---

Banyak developer punya mesin yang cukup bertenaga. Tapi pilus juga harus nyaman dipakai di perangkat yang lebih sederhana. Karena itu, penggunaan sumber daya yang rendah menjadi salah satu tujuan utama pengembangannya.

## Mesin dan baterai

Mesin utama saya adalah MacBook Neo dengan 6 core CPU (2P + 4E), RAM 8 GB, dan SSD 256 GB. Prosesornya dekat dengan keluarga chip A di iPhone. Baterai jadi salah satu hal pertama yang saya perhatikan saat mengembangkan pilus.

Semakin sedikit kerja CPU yang tidak perlu, semakin baik daya tahan baterainya. Memori punya pertimbangannya sendiri. SSD di MacBook Neo memberi ruang untuk memindahkan sebagian data kerja ke disk, dengan konsekuensi performa yang perlu diperhitungkan.

## CPU

Saya melihat tiga keadaan: aktif di depan, aktif di latar belakang, dan idle. pilus punya dua mode tampilan untuk kebutuhan itu: jendela dan menu bar.

Untuk patokan, penggunaan CPU 600% berarti enam core penuh. Di sini 1 mCPU berarti 0,1% dari satu inti.

- Aktif di depan: sekitar 300 mCPU (30% dari satu core)
- Aktif di latar belakang: sekitar 100 mCPU (10% dari satu core)
- Idle: sekitar 30 mCPU (3% dari satu core)

Angkanya bisa berubah-ubah. Sasaran pilus adalah tetap di bawah 500 mCPU saat workload peak selama lima detik. Kalau pemakaian melewati itu, ada yang perlu dibenahi meski harus mengorbankan fitur.

## Memori

Target penggunaan memori adalah sekitar 80 MB RSS, dengan puncak di bawah 120 MB selama lima detik.

## Disk

Aplikasi macOS berukuran sekitar 50 MB.

## Menjaga pilus tetap ringan

Angka target hanya berguna kalau kebiasaan pengembangannya mendukungnya. Ini beberapa aturan yang saya pegang:

- Jendela yang diam harus benar-benar diam. Agent yang sedang bekerja ditandai titik oranye, bukan spinner yang menggambar ulang jendela 30 kali per detik. Cursor/caret juga tidak berkedip secara default.
- Polling melambat lima kali saat pilus berada di belakang aplikasi lain, lalu kembali normal saat kamu kembali.
- Tab di panel kanan hanya mengambil data saat terlihat. Port, GitHub, dan Git tidak diperiksa saat tabnya tertutup.
- Pekerjaan jaringan, SSH, `gh`, dan disk berjalan di thread terpisah. Thread GUI tidak pernah menunggunya.

Untuk melihat apa yang sedang dikerjakan pilus, buka **Process** dari command palette. Halaman itu menampilkan CPU, memori, tiap thread, dan panggilan keluar terakhir. Datanya diperbarui sekali per detik selama halaman terbuka.

**Energy mode** mematikan animasi, membuat sudut jendela persegi, dan menghilangkan transparansi. Pakai saat baterai hampir habis atau Mac sedang sibuk.

Atau sedang WFC dan lupa membawa charger.
