---
title: Si pilus
description: sipil adalah daemon agent yang bisa kamu ajak ngobrol dari pilus, dan yang menjalankan coding agent atas namamu.
sidebar:
  hidden: true
---

sipil adalah agent yang tinggal di mesin sendiri dan bisa kamu ajak ngobrol dari pilus. Ia bukan
bagian dari pilus. Ia daemon Node terpisah ([kepelet/sipil](https://github.com/kepelet/sipil)) di atas
`pi-durable`, dan pilus hanya jendelanya.

Bedanya dengan chatbot biasa: percakapan, giliran model, dan pemanggilan tool ditulis ke SQLite
sebelum ada yang melihatnya. Kalau sipil mati atau Mac restart, ia melanjutkan dari titik berhenti,
bukan mulai dari nol.

## Agent bawaan

Satu instalasi sipil membawa satu agent: **sipil**, si chief of staff. Tugasnya menerima permintaan,
mengerjakan hal yang konkret sendiri, dan menyalakan coding agent di workspace pilus.

Katakan "tambahkan basic auth ke dashboard sipil". Ia melihat workspace mana yang terbuka, coding
agent apa yang terpasang, lalu menjalankan satu di workspace itu dengan seluruh tugasnya sebagai
prompt. Pekerjaannya muncul di pilus seperti agent lain. Detailnya ada di [Connector](/sipil/connector)
dan [Tools](/sipil/tools).

Mau agent lain? Tulis sebuah file Markdown di `~/.sipil/agents/`. Lihat [sipil to sipil](/sipil/s2s).

## Di mana ia muncul

- Sidebar punya bagian **SIPIL**. Isinya agent, kontak, dan channel. Titik hijau berarti daemonnya
  hidup, titik abu-abu berarti ia sedang tidak ada.
- Ikon pilus berkontur di toolbar kanan membuka tab Sipil.
- Klik satu baris dan halaman chat terbuka. Tampilannya ala IRC: `waktu nick: teks`, tool call sebagai
  baris `*` yang bisa dilipat, dan kolom input satu baris tanpa tombol Send.
- Tombol info di header membuka About: nama, host, model, instruksi, dan tool agent itu. Tombol
  Work membuka papan pekerjaan yang sedang dipegangnya.

Daemon yang sedang mati tetap bisa dibuka. Percakapan terakhirnya disimpan di Mac kamu, dan pesan
yang kamu tulis menunggu di antrean sampai daemon kembali.

## Menyambungkan

Tidak ada kata sandi, URL, atau port. Sambungannya lewat [iroh](https://iroh.computer) dan tiap
perangkat dikenali dari device ID-nya.

1. Di pilus, buka tab Sipil dan nyalakan **Let contacts reach this pilus**, lalu salin device ID
   atau ticket-nya.
2. Di mesin tempat sipil berjalan:

   ```sh
   npm run connect -- --device-id <device id pilus>
   ```

3. pilus menampilkan "sipil wants to connect" di bawah Contacts. Terima, selesai.

Selama belum kamu terima, tidak ada yang diizinkan, jadi device ID yang salah ketik tidak
memberi akses ke siapa pun.

Perlakukan allow-list seperti kunci SSH. Perangkat yang diizinkan bisa menyuruh agent membaca dan
menulis file serta menjalankan shell sebagai kamu, memakai API key kamu.

## Key dan model

Key dan model urusan sipil, dibaca dari environment mesin yang menjalankannya
(`OPENCODE_API_KEY`, `SIPIL_MODEL`). pilus tidak bisa membaca, mengubah, atau memilihkan keduanya,
jadi tidak ada form key di sini. Halaman chat hanya menampilkan model yang sedang dipakai.

## Kontak dan channel

sipil juga jalan antar-pilus: tambah kontak, kirim pesan langsung, bikin channel, dan sebut
`@NamaAgent` supaya agent ikut menjawab. Semuanya tanpa server, dan detailnya ada di
[sipil to sipil](/sipil/s2s).
