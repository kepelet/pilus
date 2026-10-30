---
title: "Memperkenalkan: pilus"
type: blog
date: 2026-11-01
---

Saat mengembangkan pilus, pertanyaan "kapan pilus akan dirilis?" selalu menghantui saya karena pada dasarnya saya sudah menggunakan
pilus dikeseharian dan saya rasa sudah cukup oke untuk bisa digunakan oleh publik. 

Hari ini (30 Oktober 2026), herdr [mengumumkan](https://x.com/herdrdev/status/2115841099575222765) aplikasi GUI nya,dan  yang sebenarnya,
sudah saya ehm "prediksi" setelah mendapatkan pendanaan sebesar $6M dari YCombinator untuk kemana herdr akan berjalan.

100 milyar rupiah.

Setelah melihat pengumuman tersebut, saya menjadi lebih yakin bila membuat GUI client untuk luvus adalah langkah yang tepat.

Setidaknya untuk saya.

## Berlomba menjadi yang tercepat

Membuat dan mengembangkan *software*, hari ini menjadi lebih mudah. Hampir semua orang dapat mengembangkan dan menjalankan software
untuk dirinya sendiri ataupun orang lain.

Sebagian orang, berlomba dalam membuat software yang cepat. Yang lain, ada yang berlomba dalam harga.

Saya memilih langkah yang berbeda untuk pilus: saya ingin membuat software yang sangat efisien.

Software yang dapat diandalkan di hardware medioker ataupun di jaringan Indihome.

## Bukan terminal emulator

pilus tidak mengemulasi terminal karena pekerjaan itu sudah dilakukan oleh luvus. Jika dianalogikan, bayangkan seperti bagaimana
peramban menerima HTML, mem-parsing, dan pada akhirnya menampilkan dokumen web. pilus ada di lapisan seperti peramban tersebut. 

Jika pilus dirasa cepat, pada dasarnya itu adalah hasil karya luvus.

Luvus adalah program TUI komplit yang berjalan di terminal dan pilus menggambarkannya dengan GPUI menggunakan Metal sebagai
backend nya di macOS.

Di platform selain macOS seperti GNU/Linux, backend yang digunakan bisa OpenGL ataupun Vulkan berkat wgpu. Yang, sekali lagi,
bukanlah karya yang dibuat oleh pilus.

Yang berarti, kemungkinan, ironisnya pilus tidak bisa berjalan di komputer tanpa GPU.

## Pain points

Versi alpha dari pilus adalah untuk menghadapi masalah yang biasa saya temukan di terminal client pada umumnya dan khususnya kapabilitas
dari IDE.

Setelah sebagian besar masalah tersebut teratasi, maka pilus akan dipromosikan ke versi beta.

### Terminal multiplexer

Masalah utama yang akan dihadapi oleh awam dalan menggunakan terminal multiplexer, yang saya yakin ada 3:

- Scrollback
- Clipboard
- Rendering

Meskipun, yang saya yakin, terminal multiplexer versi yang lebih modern sudah menyelesaikan masalah-masalah tersebut.

Scrollback dapat dilakukan via mouse scroll.

Copy-paste teks yang ada di layar seharusnya dapat bekerja.

Dan, uh, yang paling sulit: text rendering, pewarnaan, unicode/glyph dan screen flickering/redraw.

Beberapa masalah sudah coba dihandle oleh pilus dengan cara yang agak kotor, dan beberapa masalah mungkin masih ada,
yang mengharuskan perubahan di lapisan daemon atau luvus.

Namun bagaimanapun, pilus akan selalu melakukan optimasi sampai mendapatkan pengalaman pengguna yang terbaik.

### File management

Berdasarkan riwayat dari [atuin](https://atuin.sh) di salah satu komputer yang sering saya gunakan, ada ratusan
kali (366) saya menjalankan scp, dan sebagai perbandingkan: top command saya adalah `ls` sebanyak 4939, `ssh`
sebanyak 3252x, `cd` 2319, `ping` 1969x, dan seterusnya.

```bash
docs/05-blog on  main[$!?⇡] atuin stats; echo
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

docs/05-blog on  main[$!?⇡] atuin search --cmd-only --search-mode prefix scp | wc -l
      366
```

Yang bisa dikatakan, saya cukup sering memindahkan berkas antar komputer, atau setidaknya sehari sekali! 

Meskipun terkadang saya menggunakan Airdrop dan Taildrop sebelum menggunakan pilus ataupun ke komputer yang tidak
bisa dijangkau melalui pilus.

pilus menawarkan manajemen file sederhana: upload, download, rename, hapus dan semacamnya untuk alasan kenyamanan. Juga, mendukung
upload via clipboard untuk kenyamanan bekerja dengan coding agent di remote machine sekalipun :))

Beberapa operasi dilakukan melalui luvus UHP dan beberapa melakukan, ehm, RCE (SSH), untuk operasi yang belum/tidak didukung oleh luvus. 

## Yang sudah jalan

Versi terbaru adalah [v0.3.0](/changelog/v0-3-0). Sehari-hari saya memakainya untuk:

- menjalankan banyak coding agent di Mac sendiri dan di mesin SSH, dalam satu daftar
- membaca diff, memberi catatan per baris, lalu mengirim catatannya ke agent
- membuka `localhost:3000` di browser kecil di sebelah agent, tanpa pindah aplikasi
- menerima notifikasi saat agent menunggu jawaban atau selesai, bahkan ketika jendela sudah ditutup
- mengintip dan menjawab agent dari iPhone lewat [aplikasi iOS](/features/ios)

## Yang belum

- Hanya macOS di Apple Silicon. Linux belum ditentukan.
- Luvus harus sudah terpasang dan CLI `luvus` ada di `PATH`. pilus tidak membawanya.
- Tab bar dan status line dari Luvus belum bisa diambil alih klien, jadi pilus menutupinya dengan versi
  native. Alasannya ada di [Why a native client](/blog/why-a-native-client).
- Aplikasi iOS baru tersedia lewat TestFlight, di iOS 17 ke atas.

## Artinya alpha

Ada yang bisa rusak, dan sebagian akan rusak. Saya memakai pilus setiap hari untuk membangun pilus,
jadi masalah yang menghalangi saya biasanya cepat diperbaiki. Masalah yang tidak saya temui sendiri
baru saya tahu kalau kamu memberi kabar.

Pembaruan berjalan otomatis. Di **Settings → UPDATES** kamu bisa memilih channel `stable` untuk rilis
bertag, atau `preview` untuk build nightly yang lebih cepat sampai tapi lebih kasar. Setiap unduhan
harus punya tanda tangan minisign yang valid sebelum dipasang.

## Mulai dari mana

1. [Unduh pilus](https://usepilus.dev/download), lalu ikuti [Instalasi](/installation).
2. Lanjut ke [Memulai](/quickstart) untuk menghubungkan pilus ke Luvus.
3. Kalau ingin memantau dari ponsel, pasang aplikasi iOS dan pasangkan lewat
   **Settings → Remote Access**.

Kalau ada yang rusak atau terasa aneh, saya ingin tahu. Alpha berguna kalau ada yang melaporkannya.
