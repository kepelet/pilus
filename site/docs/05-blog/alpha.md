---
title: "Memperkenalkan: pilus"
type: blog
date: 2026-11-01
---

Saat mengembangkan pilus, pertanyaan "kapan pilus akan dirilis?" selalu menghantui saya karena pada dasarnya saya sudah menggunakan pilus di keseharian dan saya rasa sudah cukup oke untuk bisa digunakan oleh publik.

Hari ini (30 Oktober 2026), [herdr](https://herdr.dev) [mengumumkan](https://x.com/herdrdev/status/2115841099575222765) aplikasi GUI-nya. Ini sebenarnya sudah saya, ehm, "prediksi" sejak herdr mendapat [pendanaan seed sebesar $6M](https://herdr.dev/blog/herdr-raised-a-seed/): ke mana pengembangan herdr akan berjalan, dan khususnya ke mana uang tersebut akan "dibakar".

Catat: 100+ miliar rupiah.

Itu pun dengan asumsi kurs 17.000, my fellow Indonesian!

Setelah melihat pengumuman tersebut, saya menjadi lebih yakin bahwa membuat GUI client untuk [luvus](https://luvus.dev/) adalah langkah yang tepat.

Setidaknya untuk saya.

Alasannya? Entahlah, mungkin pilus juga akan mendapatkan pendanaan 100 miliar rupiah?

HAHA.

## Karena semuanya berlomba menjadi yang tercepat

Membuat dan mengembangkan software hari ini menjadi lebih mudah. Hampir semua orang dengan komputer dan akses internet dapat mengembangkan dan menjalankan software untuk dirinya sendiri ataupun orang lain.

Sebagian orang berlomba dalam membuat software yang cepat.

Yang lain berlomba dalam harga.

Saya memilih langkah yang berbeda untuk pilus: saya ingin membuat software yang sangat efisien.

Software yang dapat diandalkan di hardware medioker ataupun di jaringan IndiHome.

Di MacBook Neo citrus tercinta saya ataupun di Steam Deck yang jarang sekali digunakan untuk game.

## Bukan terminal emulator

pilus tidak mengemulasi terminal karena pekerjaan itu sudah dilakukan oleh luvus. Jika dianalogikan, bayangkan sebuah
peramban yang menerima DOM yang sudah jadi, lalu tugasnya hanya menggambar dokumen web ke layar. pilus ada di lapisan seperti peramban tersebut.

Jika pilus dirasa cepat, pada dasarnya itu adalah hasil karya luvus.

luvus adalah program TUI lengkap yang berjalan di terminal dan pilus merendernya dengan [GPUI](https://www.gpui.rs) menggunakan [Metal](https://developer.apple.com/metal/) sebagai
backend-nya di macOS.

Di platform selain macOS seperti GNU/Linux, backend yang digunakan bisa OpenGL ataupun Vulkan berkat [wgpu](https://wgpu.rs). Yang, sekali lagi,
bukanlah karya yang dibuat oleh pilus juga.

Yang berarti, kemungkinan, ironisnya, pilus tidak bisa berjalan di komputer tanpa GPU, padahal di sisi lain pilus menjual "lightweight" sebagai fitur utamanya.

## Pain points

Versi alpha pilus dibuat untuk menjawab masalah yang biasa saya temukan di terminal client pada umumnya, dan khususnya keterbatasan
kapabilitas IDE.

Setelah sebagian besar masalah tersebut teratasi, mungkin pilus akan dipromosikan ke versi beta.

### Terminal multiplexer

Masalah utama yang akan dihadapi oleh awam dalam menggunakan terminal multiplexer, kemungkinan, ada tiga:

- Scrollback
- Clipboard
- Rendering

Meskipun, yang saya yakin, terminal multiplexer versi yang lebih modern sudah menyelesaikan masalah-masalah tersebut.

Scrollback dapat dilakukan via mouse scroll.

Copy-paste teks yang ada di layar seharusnya dapat bekerja.

Dan, uh, yang paling sulit: text rendering, pewarnaan, Unicode/glyph dan screen flickering/redraw.

Beberapa masalah sudah coba di-handle oleh pilus, dengan cara yang mungkin benar ataupun cara yang agak kotor. Beberapa masalah mungkin masih ada, termasuk yang mengharuskan perubahan di lapisan daemon alias luvus.

Bagaimanapun, pilus akan selalu melakukan optimasi sampai mendapatkan pengalaman pengguna yang terbaik.

Dan versi alpha diharapkan dapat mengumpulkan isu-isu tersebut.

### File management

Berdasarkan riwayat dari [atuin](https://atuin.sh) di salah satu komputer yang sering saya gunakan, ada ratusan
kali (366) saya menjalankan scp, dan sebagai perbandingan: top command saya adalah `ls` sebanyak 4.939 kali, `ssh`
sebanyak 3.252 kali, `cd` 2.319 kali, `ping` 1.969 kali, dan seterusnya.

```bash
docs/05-blog on  main[$!?⇡] atuin stats; echo
[▮▮▮▮▮▮▮▮▮▮] 4939 ls
[▮▮▮▮▮▮    ] 3252 ssh
[▮▮▮▮      ] 2319 cd
[▮▮▮       ] 1969 ping
[▮▮▮       ] 1747 curl
[▮▮▮       ] 1618 git status
[▮▮        ] 1459 cat
[▮▮        ] 1210 dig
[▮▮        ] 1159 gleam
[▮▮        ] 1018 hx
Total commands:   46484
Unique commands:  14505

docs/05-blog on  main[$!?⇡] atuin search --cmd-only --search-mode prefix scp | wc -l
      366
```

Yang bisa dikatakan, saya cukup sering memindahkan berkas antar komputer!

Di luar itu, terkadang saya masih menggunakan AirDrop dan [Taildrop](https://tailscale.com/kb/1106/taildrop), khususnya untuk komputer yang tidak
bisa dijangkau melalui pilus.

pilus menawarkan manajemen file sederhana: upload, download, rename, hapus dan semacamnya untuk alasan kenyamanan. Juga, mendukung
upload via clipboard, jadi bekerja dengan coding agent di remote machine sekalipun tetap nyaman :))

Beberapa operasi dilakukan melalui luvus UHP dan beberapa lainnya melakukan, ehm, RCE (SSH), untuk operasi yang belum/tidak didukung oleh luvus.

pilus juga menawarkan fitur penyunting file sederhana. Jangan menganggapnya selevel editor kelas dunia (setidaknya untuk sekarang lol) seperti [vim](https://www.vim.org), [nano](https://www.nano-editor.org), [Zed](https://zed.dev), [Helix](https://helix-editor.com) dan semacamnya.

Tapi setidaknya, saya menulis postingan blog ini di editor pilus!

### Code review

Saya masih melakukan sedikit code review. Setidaknya untuk memberikan komentar kepada agent. luvus memiliki fitur untuk membuat catatan dan mengirimkannya kepada agent dan pilus membuatnya menjadi sedikit lebih *friendly*.

Terkadang saya berpikir bahwa code review sudah tidak relevan. Tapi terkadang saya mempelajari hal baru dari solusi (ataupun "hack") yang dilakukan oleh si agent, dan terkadang pun, agent mempelajari sesuatu berdasarkan catatan yang saya buat.

Btw coba hitung ada berapa kata 'terkadang' di bagian ini?

## Fitur lain

Ada beberapa fitur tambahan yang cukup membantu (setidaknya bagi saya!) membuat proses pengembangan (khususnya web development) menjadi lebih mudah:

- Port Forwarder
- In-app browser
- Integrasi GitHub

Fitur ini masih cukup basic, bahkan untuk in-app browser masih belum bisa digunakan bila mengakses situs menggunakan protokol non-HTTPS.

Jujur, fitur in-app browser saya gunakan untuk membuka YouTube dan menonton video sambil menunggu si agent bekerja. Kadang juga untuk membuka [Hacker News](https://news.ycombinator.com).

Untuk integrasi dengan GitHub? Entahlah, saya rasa saya hanya impulsif ketika memulainya.

## Masalah yang sudah diketahui

- Rendering terminal belum sempurna: pewarnaan, Unicode/glyph, dan screen flickering masih bisa bermasalah, dan sebagian mungkin butuh perubahan di luvus
- In-app browser belum bisa membuka situs non-HTTPS (`localhost` dan IP tidak terpengaruh)
- Editor belum punya swap file. Kalau kamu dan agent menyunting file yang sama, pilus hanya memberi tahu setelah file berubah di disk, tidak menggabungkannya
- Mesin SSH yang butuh kata sandi atau passphrase saat login tidak akan tersambung, dan versi luvus di sana harus cocok dengan yang di Mac kamu
- Search dan Insights baru mengenali sesi Claude dan Pi
- Hanya macOS di Apple Silicon, dan tanpa GPU pilus kemungkinan tidak bisa berjalan
- Aplikasi iOS masih lewat TestFlight

Daftar yang lebih rinci, lengkap dengan cara mengakalinya, ada di [FAQ](/faq#masalah-yang-sudah-diketahui).

## Roadmap

Ada beberapa hal yang sudah ada di backlog dan pada dasarnya "one prompt away" untuk dilakukan, seperti:

- Swap file saat mengedit berkas
- Conflict-free editor untuk berkolaborasi dengan agent
- Aplikasi iOS (masih internal TestFlight dengan nama "pilus mobile connect")
- Integrasi dengan sipil ("si pilus" alias bot pribadi)
- Dukungan di OS lain seperti GNU/Linux (saya sudah mencobanya di X11/Wayland dan berjalan dengan baik), atau Windows dan \*BSD (belum ada kapasitas untuk memeliharanya)

Namun fokus untuk sekarang (sampai ke Beta) adalah stabilitas. Performa tetap menjadi prioritas dalam pengembangan pilus, tetapi tujuan utamanya adalah membuat pilus sangat berguna. Patokan pribadi saya dalam menggunakan tools adalah seberapa *capable* tools tersebut dapat men-deliver kode ke pengguna akhir.

Termasuk untuk si aplikasi pilus ini.

## Penutup

Sebagai catatan, beberapa module yang ada di luvus mungkin tidak akan ~~berjalan~~ ditampilkan di pilus, dan ini by design. Untuk saat ini, saya tidak berencana untuk membuat sistem module/plugin sampai pilus dianggap cukup stabil.

Karena masih alpha, beberapa fitur mungkin broken, atau ada UX yang kurang baik. Saya tidak menjalankan pelacak apa pun di sini, bahkan saat pilus crash. Jadi, jika menemukan bug, apalagi crash, feel free untuk [membuat issue](https://github.com/kepelet/pilus/issues) di GitHub.

Sebagai penutup, pembaruan aplikasi dilakukan secara Over The Air (OTA). Pemeriksaan versi terbaru secara otomatis nonaktif by default (opt-in) dan silakan aktifkan opsi tersebut jika memang perlu. Paket pembaruan ditandatangani dengan [minisign](https://jedisct1.github.io/minisign/) agar yang dimodifikasi bisa terdeteksi dan ditolak, sedangkan aplikasinya ditandatangani dengan [Developer ID](https://developer.apple.com/developer-id/) dan [di-notarize](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution) oleh Apple agar [Gatekeeper](https://developer.apple.com/support/gatekeeper/) tidak cerewet.

Jadi, jika sudah menggunakan luvus, pertimbangkan untuk install dan menggunakan pilus! Download [di sini](https://usepilus.dev/download).

Dan jika belum, install luvus [sekarang](https://luvus.dev/) dan jangan lupa untuk mencoba pilus juga!
