---
title: Connector
description: Cara sipil menjangkau mesin kamu lewat pilus-agent, dan seberapa jauh ia boleh bertindak.
sidebar:
  hidden: true
---

sipil tidak menyentuh socket Luvus. Untuk melihat workspace atau menyalakan agent, ia lewat
**pilus-agent**, service latar yang dijalankan pilus saat Remote Access menyala. Itu juga pintu yang
dipakai [aplikasi iOS](/features/ios).

Akibatnya sipil tidak harus berjalan di mesin yang sama dengan pilus. Mac lain yang selalu hidup
sama bagusnya.

## Memasangkan

1. Di pilus: **Settings → Remote Access → Add Device**.
2. Centang **Allow this device to start work**, lalu salin tautan yang muncul.
3. Di mesin sipil, saat sipil sedang berjalan:

   ```sh
   npm run hub -- pair '<tautan tadi>'
   ```

`npm run hub` menampilkan status pairing, dan `npm run hub -- forget` melepasnya. Cabut juga
perangkatnya di pilus.

Satu pairing cukup untuk semua agent. Mereka memanggil dari device ID sipil yang bawaan.

## Level

| Level | Boleh |
| --- | --- |
| Operate | Melihat, mengirim prompt, membuka workspace, dan menyalakan agent. |
| Control | Hanya melihat dan mengirim prompt ke agent yang sudah ada. |

Tanpa centang "Allow this device to start work", sipil mendapat Control. Ia masih bisa membaca dan
mengetik ke agent yang berjalan, tapi tidak bisa menyalakan yang baru.

## Mesin SSH

Semua tool sipil menerima parameter `machine`: `local` untuk Mac pilus, atau salah satu mesin SSH
yang tersimpan di pilus. Di sini Operate bukan main-main. Di mesin SSH, Operate berarti otoritas penuh
sebagai kamu di sana, karena pilus-agent meneruskannya lewat `luvus uhp proxy`. Beri hanya kalau memang perlu.

## Yang dicatat

- pilus-agent mencatat setiap tulisan yang dilakukan sipil, atas nama perangkatnya.
- Batasnya 120 panggilan per menit untuk semua perangkat sekaligus. Karena itu sipil membaca daftar
  agent satu kali per mesin dan membaginya ke semua watch, bukan satu panggilan per watch.
- `sipil tell` tidak ikut ke mesin lain. Perintah itu menitipkan file di folder data sipil, jadi hanya
  jalan kalau coding agent-nya berada di mesin yang sama dengan sipil. Di tempat lain, sipil mengandalkan
  watch.
