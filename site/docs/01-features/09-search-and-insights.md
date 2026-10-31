---
title: Search & Insights
description: Cari sesi agent lama, lanjutkan atau fork, dan lihat pemakaian token. Nonaktif sampai kamu mengizinkannya.
sidebar:
  icon: search
---

Search dan Insights memakai indeks sesi coding agent dari Mac kamu dan mesin SSH yang kamu pilih. Kalau kamu pernah berpikir, "Kemarin saya minta agent mengerjakan apa, ya?" Search bisa membantu menemukannya, mungkin.

## Izin dulu

Indeks membaca transkrip sesi, yang bisa berisi apa saja. Karena itu, fitur ini **mati secara default**:

- pilus tidak membaca transkrip, lokal maupun lewat SSH, sebelum kamu menekan **Allow indexing** di halaman Search.
- Mesin remote baru dibaca setelah kamu mencentangnya. Kalau menghapus centang, sesi dari mesin itu dikeluarkan dari indeks.
- Mematikan indeks akan menghapus seluruh `index.db`.
- Indeks menyimpan judul dan metadata saja, bukan isi pesan.

Saat ini, indeks mengenali sesi Claude dan Pi.

## Search

Halaman Search menampilkan semua sesi. Kamu bisa menyaringnya berdasarkan mesin atau agent, lalu mencari lewat kolom di atas. Pilih sesi untuk membukanya.

### Halaman sesi

Halaman sesi membaca transkrip dari file saat dibuka dan tidak menyimpannya setelah itu. Untuk sesi panjang, pilus mulai dari 4 MB terakhir lalu memuat bagian sebelumnya saat kamu menggulir ke atas. Jadi, transkrip besar tidak perlu dibaca sekaligus kecuali kamu memang mencari bagian awalnya.

Di halaman ini ada dua pilihan:

- **Continue** membuka workspace sesi dan melanjutkan percakapannya.
- **Fork** memulai sesi baru dari percakapan itu, menggunakan opsi fork milik agent tersebut.

Kalau daemon tidak mengenali sesi yang bisa dilanjutkan, pilus menjalankan agent dengan `--resume <id>` jika agent itu mendukungnya. Saat ini, itu berlaku untuk Claude.

## Insights

Insights menghitung pemakaian token dari indeks yang sama, dengan izin dan pilihan mesin yang sama. Pilih rentang 7, 30, atau 90 hari, atau **All time**. Hasilnya juga bisa disaring berdasarkan mesin.

- **Tokens** menampilkan input, output, cache read, dan cache write, beserta jumlah sesi dan hari aktif.
- **Activity** menampilkan heatmap aktivitas harian selama setahun.
- **Prompts by hour** merangkum aktivitas ke 24 jam dan menyorot jam tersibuk. Yang dihitung adalah giliran model, bukan prompt. Satu prompt bisa mendapat beberapa jawaban selama agent menjalankan loop kerjanya.
- **Top agents**, **Top models**, dan **Top projects** merangkum sesi berdasarkan agent, model, dan proyek.
- **Biaya** memakai angka dari transkrip jika tersedia, seperti pada Pi. Kalau tidak ada, seperti pada Claude, pilus memperkirakan biaya dari harga publik saat halaman dibuka. Model yang harganya tidak diketahui tetap dihitung tokennya, tetapi tidak masuk total dolar. Angka ini bukan tagihan; hanya perkiraan dari jumlah token yang dilaporkan transkrip.
- Hari mengikuti zona waktu lokalmu.

Indeks hanya membaca transkrip yang sudah ada di disk. pilus tidak menjalankan agent atau menghubungi server untuk membuat Insights. Untuk mesin SSH, pilus membaca file di mesin itu dengan `grep` dan `tail`.
