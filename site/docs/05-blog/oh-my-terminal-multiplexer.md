---
title: Oh My Terminal Multiplexer!
type: blog
date: 2026-10-17
description: Kenapa terminal multiplexer, dan perjalanan dari SSH sampai tmux.
---

Jam menunjukkan 23:37 WIB, kamu mendapatkan peringatan bahwa aplikasi web yang kamu jalankan mati.

Kamu membuka terminal, menjalankan SSH, dan membuka log untuk mencari penyebabnya. Kamu perlu membuka
3 berkas log untuk mencari bagian yang paling relevan. Kamu buka 2 tab baru, menjalankan SSH yang sama,
lalu ulangi. Setelah merasa mengetahui penyebabnya, kamu membuka tab baru lagi untuk menjalankan aplikasinya;
menjalankan SSH lagi, ulangi, sambil memonitor log baru yang muncul.

2 menit berlalu, tidak ada anomali terlihat. Bahkan tidak ada log baru yang muncul! Kamu merasa lega, sekaligus
curiga. Kamu menekan tombol enter 2x untuk memastikan. 

Tidak ada respon apapun.

Tidak lama kemudian tab pertama menampilkan pesan bahwa koneksi terputus. Begitupula sampai tab 4.

Lalu kamu teringat bahwa kamu menggunakan layanan internet dari penyedia bernama Indihouse.

Setelah 16 detik menjalankan `ping 1.1.1.1`, baris Request timeout for icmp_seq sudah tidak muncul di layar.

Lalu kamu mengulangi perintah terakhir per-tab di terminal dengan menekan panah atas, dan mencari
baris terakhir yang kamu lihat sebelumnya di log.

Koneksi terputus lagi.

Lalu kamu kepikiran untuk membuat aplikasi web menggunakan Laravel untuk bisa membaca log secara real-time, dan
bisa menyalakan ulang aplikasi melalui halaman web. 

Setelah scaffold project, kamu teringat bahwa perlu menambahkan autentikasi agar tidak sembarang orang dapat mengakses. Basic
Auth pun cukup, seharusnya.

Lalu kamu teringat bahwa harus mengatur reverse proxy juga untuk melakukan TLS Termination.

Mempertimbangkan kompleksitas tersebut, kamu kepikiran untuk memasang jaringan overlay saja untuk mempermudah segala hal.
Kamu teringat IPsec, dan Strongswan.

Yang tentu saja memilih Wireguard.

Aplikasi buatanmu berjalan di interface `wg0`. Kamu menyalakan Wireguard, dan menajalankan `ping` ke server tersebut.

Request timeout?

Melihat status koneksi Wireguard, handshake terakhir sekitar 23 detik yang lalu. Apa ada masalah di packet filter? Apakah
hole-punching nya gagal? Routing table? Salah IP? Hidup jokowi?

Bermenit-menit berlalu dan kamu menatap layar laptop dengan tatapan kosong. Kamu harus menunggu 2 jam untuk bisa bertanya
kembali ke Claude untuk rekomendasi langkah selanjutnya yang harus diambil.

Lalu kamu mendengar bisikan yang hanya berbunyi "tmux" di kejauhan. Kamu mengira-ngira: temuks? timuks? timaks?

Dan iseng menjalankan `tmux` di server yang terhubung melalui SSH. Hanya shell biasa dengan latar belakang hijau di bagian bawah dengan tulisan `[0] 0:bash`.

Kamu penasaran dengan teks 0:bash dan menjalankan `man tmux` untuk mencari tahu:

```
tmux(1) - terminal multiplexer

tmux is a terminal multiplexer: it enables a number of terminals to be created,
accessed, and controlled from a single screen. tmux may be detached from a
screen and continue running in the background, then later reattached.

When tmux is started it creates a new session with a single window
and displays it on screen. A status line at the bottom of the screen shows
information on the current session and is used to enter interactive commands.

...
```

Responmu hanyalah *wait, what?*

Lalu kamu mulai mempelajari sedikit tentang tmux. Menjalankan `ctrl-b c` sebanyak 8x,
menjalankan `htop` di 4 panel, close terminal, SSH lagi, `tmux a`, dan 4 htop masih berjalan.

Kamu bahkan mematinyalakan Wi-Fi di komputer untuk memastikan jika hasilnya masih sama. 

Dan ternyata masih.

Semenjak hari itu `tmux` menjadi teman terbaikmu. Kamu bahkan sudah malas membuat
systemd unit hanya untuk menjalankan program di background.

tmux all the way!

Lalu kamu mencoba menjalankan claude code di tmux, namun kali ini di komputer lokal.

Panel 1 claude code, panel 2 npm run dev, panel 3 lazygit.

Panel 4 btop untuk screenshot.

Dan kamu setuju bahwa SSH adalah protokol terbaik untuk mengakses komputer secara jarak jauh,
dan tmux, the terminal multiplexer, membuatnya sempurna.

Terima kasih, nicm!
