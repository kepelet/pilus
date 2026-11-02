---
title: FAQ
description: Pertanyaan yang mungkin sering muncul.
sidebar:
  icon: circle-help
---

## Apakah pilus menggantikan Luvus?

Tidak. Luvus adalah daemon yang memegang sesi, workspace, dan agent. pilus hanya klien yang
menggambar permukaannya di jendela native. Tanpa Luvus, pilus tidak punya apa-apa untuk ditampilkan.

## Apa yang terjadi saat jendela ditutup atau Mac tidur?

Luvus tetap berjalan, jadi agent juga. Dan pilus tinggal terhubung lagi. Koneksi ke mesin SSH yang
putus akan dicoba ulang secara otomatis.

## Platform apa yang didukung?

macOS di Apple Silicon (macOS 13 ke atas). iOS dan iPadOS masih di internal TestFlight.

GNU/Linux belum dirilis; saya sudah mencobanya di X11 dan Wayland dan berjalan dengan baik,
tapi belum ada kapasitas untuk memeliharanya.

Belum ada rencana untuk Windows dan \*BSD.

## Apalah bisa jalan di komputer tanpa GPU?

Kemungkinan tidak.

pilus menggambar dengan GPUI lewat Metal, dan di platform lain lewat wgpu (OpenGL atau Vulkan).

## Seberapa berat pilus?

Ini target dan bukan hasil benchmark. Angkanya bisa bervariasi:

- CPU: kurang dari 500 mCPU saat peak selama 5 detik, sekitar 30 mCPU saat idle
- Memory: sekitar 80 MB RSS, peak di bawah 120 MB
- Disk: sekitar 50 MB untuk aplikasinya

Jika pilus melewati itu, akan ada yang harus diperbaiki, sekalipun artinya menghapus fitur.

## Apakah pilus mengirim data ke GitHub atau server lain?

Tidak ada pelacak atau telemetri, bahkan saat pilus crash. Satu-satunya yang terjadi adalah
panggilan keluar yang kamu minta sendiri, atau yang jelas diperlukan:

- **GitHub**: bagian Repositories memakai `gh` di Mac kamu sendiri, tidak lewat daemon atau SSH,
  dan semua permintaan di-cache. Matikan di Settings dan tidak ada permintaan sama sekali.
- **Pembaruan**: manifest dari `releases.kepelet.com` yang saat ini berada di jaringan Cloudflare. Terjadi hanya saat kamu
  memeriksa atau menyalakan pemeriksaan otomatis (mati secara default).
- **Luvus**: diunduh dari rilisnya di GitHub saat dipasang atau diperbarui, lalu dicek dengan checksum.
- **Notifikasi push iOS**: hanya kalau kamu memasangkan ponsel. Lihat [aplikasi iOS](/features/ios).

Peringatan dan crash dicatat di `~/.pilus/logs/pilus.log` di Mac kamu. Berkas tersebut tidak pernah dikirim kemanapun.

## Apa itu Search, dan apakah pilus membaca percakapan saya?

Search mengindeks judul dan metadata sesi Claude dan Pi di mesin yang terhubung dengan luvus.

Fitur ini **mati sampai kamu mengizinkannya**, mesin remote dibaca hanya kalau kamu mencentangnya, dan isi
pesan tidak disimpan di indeks. Matikan lagi dan indeksnya akan dihapus.

Detailnya ada di [Search & Insights](/features/search-and-insights).

## Agent apa saja yang bisa dipakai?

Apa pun yang bisa dijalankan Luvus di terminal. pilus tidak memilihkan agent: ia hanya menampilkan
dan menjalankannya lewat Luvus.

Tapi untuk Search dan Insights, saat ini hanya sesi Claude dan Pi yang dikenali.

## Di mana pengaturan disimpan?

Semuanya di folder `~/.pilus`, yang hanya bisa dibaca pemiliknya:

| Berkas | Isi |
| --- | --- |
| `settings.json` | Preferensi: font, tema, channel pembaruan, dan sejenisnya. Aman diedit tangan. |
| `pilus.db` | Keadaan jendela, pin workspace, lipatan folder, catatan markdown, dan cache GitHub. |
| `index.db` | Cache untuk Search. Boleh dihapus kapan saja karena bisa dibangun ulang. |
| `logs/pilus.log` | Peringatan dan crash. Dirotasi di 4 MB. |

## Aplikasi iOS aman?

Ponsel dipasangkan lewat kode sekali pakai yang hanya tampil di host, lalu dikenali dari
identitas iroh-nya. Tiap perangkat punya level View, Control, atau Operate dan bisa dicabut kapan saja.

Detailnya ada di [aplikasi iOS](/features/ios).

## Kenapa module Luvus tertentu tidak muncul di pilus?

By design. Beberapa module Luvus memang tidak ditampilkan di pilus, dan untuk sekarang saya tidak
berencana membuat sistem module atau plugin sampai pilus cukup stabil.

## Masalah yang sudah diketahui

Kalau kamu menemukan issue yang tidak tercantum di sini,
[buat issue](https://github.com/kepelet/pilus/issues) dan lampirkan bagian yang relevan dari
`~/.pilus/logs/pilus.log`.

### Terminal

- **Rendering belum sempurna.** Pewarnaan, Unicode/glyph, dan screen flickering atau redraw masih bisa
  bermasalah. Beberapa sudah ditangani pilus, dan sebagian lain mungkin butuh perubahan di Luvus.
- **Link `file://` dari daemon tidak terbuka.** pilus sengaja hanya menuruti permintaan membuka URL
  dengan skema `http`, `https`, dan `mailto`, karena permintaan tersebut bisa datang dari mesin remote.
  Salin path-nya, atau `cmd`-klik path file di layar untuk membukanya di editor.
- **Local echo hanya muncul di koneksi yang lambat.** Ketikan baru digambar lebih dulu kalau waktu
  balas terukur di atas 50ms. Di jaringan yang cepat, kamu tidak akan melihatnya, dan itu normal.

### Mesin SSH

- **Login yang meminta kata sandi tidak jalan.** pilus memakai `BatchMode=yes` karena tidak ada yang bisa
  ditanya. Cek dengan `ssh <host> true` dari terminal: kalau di sana meminta kata sandi, pilus juga gagal.
- **Host key harus sudah dikenal.** Sambungkan sekali dari terminal supaya masuk ke `known_hosts`.
- **Versi Luvus harus cocok.** Kalau muncul *protocol version mismatch*, gunakan **Update Luvus** pada
  mesin tersebut via Settings. Panduannya ada di [Hubungkan ke mesin remote](/guides/connect-remote).

### Editor dan file

- **Belum ada swap file atau penggabungan.** Kalau kamu dan agent menyunting file yang sama, pilus hanya
  memberi tahu setelah file berubah di disk dan menawarkan *Reload* atau *Keep Mine*.
- **File yang terlalu besar tidak dibuka untuk diedit.** Begitu juga file yang terbaca terpotong.
- **Syntax highlight terbatas.** TypeScript dan JSX dibaca sebagai JavaScript, sedangkan TOML dan
  beberapa lainnya masih tanpa warna.
- **Diagram mermaid yang sumbernya salah** tetap tampil sebagai kartu kode, supaya preview yang sedang
  kamu ketik tidak kosong.

### In-app browser

- **Situs http biasa di internet belum bisa dibuka.** `localhost` dan alamat IP tidak terpengaruh, jadi dev server
  yang diteruskan lewat [Port forwarder](/features/port-forwarder) tetap bisa dibuka. Nama di jaringan privat
  (misalnya `*.lab`) bisa diizinkan saat build dengan `PILUS_HTTP_HOSTS`. Untuk yang lain, pakai browser biasa.
- Maksimal delapan tab halaman.

### Search dan Insights

- **Hanya sesi Claude dan Pi** yang dikenali.
- **Biaya di Insights adalah perkiraan** dari daftar harga publik, bukan tagihan, dan model yang
  harganya tidak diketahui tidak masuk total dolar.
- **Prompts by hour menghitung giliran model**, bukan jumlah prompt yang kamu ketik.

### Platform dan iOS

- **Hanya macOS 13 ke atas di Apple Silicon.** Tanpa GPU, pilus kemungkinan tidak bisa berjalan.
- **Aplikasi iOS masih lewat TestFlight.** Ponsel hanya bisa menjangkau Mac yang menyala dan tidak
  tidur, karena yang melayaninya adalah service Remote Access di Mac tersebut.
