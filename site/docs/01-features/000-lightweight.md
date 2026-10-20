---
title: Lightweight
description: pilus menjadikan keringanan sebagai fitur utama
sidebar:
  icon: feather
---

Kebanyakan pengembang menggunakan mesin yang cukup powerful. Meskipun target pengguna pilus adalah
pengembang (dan power user), low footprint menjadi fitur pertama dan yang paling utama dalam mengembangkan
pilus.

## Mesin & Baterai

Mesin utama saya adalah Macbook Neo yang memiliki 6 CPU (2P + 4E), 8 GB memory, dan penyimpanan 256 GB. Terlebih,
Macbook Neo menggunakan CPU yang relatif sama dengan keluarga iPhone (A series) yang mungkin memiliki andil juga. Penggunaan
baterai menjadi ukuran pertama dalam mengembangkan pilus.

Semakin sedikit & efisien pekerjaan CPU, semakin efisien juga penggunaan baterai. Memory/RAM adalah cerita lain, dan beruntungnya,
media penyimpanan yang 256 GB itu adalah SSD. Beberapa working data bisa di *offload* ke SSD dengan tradeoff lainnya.

## CPU

Ada 3 hal yang saya perhatikan: Active (foreground), Active (background), Idle. Karena hal ini, pilus memiliki 2 mode: window dan
menu bar.

Sebagai perbandingan, penggunaan 600% CPU = 6 core. Dan untuk mempermudah, kita akan menggunakan satuan mCPU yang adalah 1 mCPU = 0,1% CPU.

- Active foreground: 300 mCPU (30% dari 1 core)
- Active background: 100 mCPU (10% dari 1 core)
- Idle: 30 mCPU (0,3% dari 1 core)

Ini bukan benchmark dan nilai sangat bervariasi. Intinya, target penggunaan CPU pilus <500 mCPU saat peak selama 5 detik. Jika lebih dari itu,
maka akan ada improvement sekalipun harus menghapus fitur.

## Memory

Target RSS (Resident Set Size) nya adalah 80 MB dengan peak <120MB selama 5 detik.

## SSD

50MB (bundled macOS app).
