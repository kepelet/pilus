---
title: Tools
description: Apa saja yang bisa dilakukan sipil, dan bagaimana ia melapor kembali.
sidebar:
  hidden: true
---

Chief of staff punya tool untuk bekerja di pilus, mengatur agent lain, dan mengingat. Agent buatanmu
memakai tool bawaan chief kalau `tools` tidak diisi. Sebutkan namanya di `tools` kalau mau tool
tertentu.

## Bekerja di pilus

Semua tool ini lewat [connector](/sipil/connector), dan semuanya menerima `machine`.

| Tool | Fungsi |
| --- | --- |
| `pilus_machines` | Daftar mesin: `local` dan mesin SSH yang tersimpan di pilus, beserta apakah siap. |
| `pilus_workspaces` | Workspace yang terbuka, lengkap dengan direktori dan agent yang berjalan di dalamnya. |
| `pilus_agents` | Coding agent yang terpasang di mesin itu. Dengan `kind`, daftar model dan thinking level-nya. |
| `pilus_open_workspace` | Buka direktori absolut sebagai workspace, atau bawa ke depan kalau sudah terbuka. |
| `pilus_start_agent` | Buka tab baru di workspace, jalankan agent, lalu ketik prompt setelah agent siap (maksimal 90 detik). |
| `pilus_watch_agent` | Awasi agent yang sudah berjalan dan beri kabar saat selesai. |
| `pilus_agent_read` | Ujung layar agent: apa yang terakhir ia katakan, tanyakan, atau cetak. |
| `pilus_agent_send` | Ketik pesan di prompt agent yang berjalan: tindak lanjut, jawaban, atau kata-kata agent lain. |

`pilus_start_agent` memakai kunci idempotensi. Balasan yang hilang tidak menyalakan agent kedua.
`model` dan `effort` diperiksa terhadap katalog agent dan ditolak, lengkap dengan yang valid, bukan
ditebak. Hanya Pi yang bisa diberi keduanya lewat nama. Untuk agent lain, chief meneruskan `args`
apa adanya.

Kalau tidak yakin workspace, agent, atau modelnya, chief bertanya satu pertanyaan pendek dan berhenti.
Tidak ada yang dinyalakan berdasarkan tebakan.

## Laporan

Agent yang dinyalakan chief diawasi oleh tugas latar yang awet. Tiap lima detik ia melihat daftar agent
di mesin itu. Saat agent selesai, berhenti di pertanyaan atau izin (`blocked`), atau keluar, sebuah
laporan berisi ujung layarnya masuk ke percakapan chief dan memulai giliran berikutnya.

"Selesai" artinya idle dengan layar yang tidak berubah selama 30 detik. Status idle saja tidak
dipercaya, karena `pi` terbaca idle saat sedang bekerja.

Setelah laporan masuk, chief memeriksa hasilnya (layar, dan `git diff` kalau direktorinya ada di mesin
yang sama), menindaklanjuti kalau ada yang kurang, lalu memberi tahu kamu. Ia tidak pernah memberi
izin yang tidak kamu berikan.

Watch bertahan dua jam secara bawaan (`watch_minutes`, maksimal dua belas) dan selamat dari restart.
Sebagai jaring pengaman, tiap 15 menit sipil melihat ulang agent yang dulu diawasi dan kembali bekerja,
paling banyak tiga kali sehari per pane.

## Pengingat dan rutinitas

| Tool | Fungsi |
| --- | --- |
| `follow_up` | Pengingat sekali untuk dirinya sendiri, 1 menit sampai 7 hari, untuk hal yang tidak akan dibawa watch. Maksimal sepuluh per percakapan. |
| `create_routine`, `list_routines`, `update_routine`, `delete_routine` | Pekerjaan berulang dengan jadwal cron lima field dan zona waktu. Maksimal 20 per agent, tidak lebih sering dari tiap lima menit. |

Tiap rutinitas berjalan di sesinya sendiri (`Routine: <nama>, <waktu>`). Kalau tidak ada yang perlu
dilaporkan, ia menjawab `NOTHING_TO_REPORT` dan diam. Rutinitas yang terlewat lebih dari 30 menit saat
sipil mati dilewati, bukan dikejar.

## Papan kerja

| Tool | Fungsi |
| --- | --- |
| `add_work`, `update_work`, `close_work` | Satu item per pekerjaan: status (`todo`, `doing`, `waiting`, `blocked`), siapa yang memegang, langkah berikutnya, dan menunggu siapa. Maksimal 30 yang terbuka. |

Papan ini ikut di prompt tiap giliran, jadi tidak hilang saat percakapan diringkas. pilus menampilkannya
di panel **Work** pada halaman agent, dengan hitungan item yang menunggu seseorang.

## Memori

| Tool | Fungsi |
| --- | --- |
| `remember`, `forget` | Satu kalimat per entri, ikut di prompt di semua percakapan. Maksimal 100 entri, 600 karakter. |

Percakapan yang konteksnya lewat 120 ribu token (`SIPIL_CONTEXT_BUDGET`) diringkas otomatis. Ringkasannya
disuruh menyimpan apa yang masih terutang: pekerjaan terbuka, agent yang sudah dinyalakan, rutinitas,
pertanyaan yang belum dijawab, dan keputusan.

## Agent lain

| Tool | Fungsi |
| --- | --- |
| `ask_agent` | Bertanya ke agent lain di sipil yang sama. Lihat [sipil to sipil](/sipil/s2s). |
| `create_agent`, `update_agent`, `delete_agent` | Menulis, mengubah, dan memindahkan file agent di `~/.sipil/agents/`. |

Ada juga `create_artifacts` untuk membuat artefak. `web_fetch` mengembalikan halaman sebagai teks,
paling banyak 20.000 karakter per panggilan, dan tidak ada mesin pencari di belakangnya.
