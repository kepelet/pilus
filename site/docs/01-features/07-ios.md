---
title: iOS app
description: Pantau dan arahkan agent dari iPhone atau iPad.
sidebar:
  icon: smartphone
---

Aplikasi pendamping ini membantumu memantau dan mengendalikan mesin dari iPhone atau iPad saat kamu jauh dari Mac. Aplikasi tersedia lewat [TestFlight](https://usepilus.dev/ios) untuk iOS 17 ke atas. Masih alpha, dan saat ini hanya tersedia lewat TestFlight.

## Memasangkan

1. Di pilus, buka **Settings -> Remote Access** lalu nyalakan fiturnya.
2. Pilih **Add device** untuk menampilkan QR berisi tautan `pilus://pair/...`.
3. Pindai QR itu dengan ponsel.

Kode pairing hanya berlaku sekali, selama sepuluh menit, dan hanya ditampilkan di host. Setelah dipasangkan, pilus mengenali ponsel lewat identitas iroh-nya. Kamu tidak perlu memasangkannya lagi.

## Koneksi

Ponsel terhubung langsung ke host lewat QUIC peer-to-peer (iroh). Kamu tidak perlu membuka port. Remote Access berjalan sebagai service latar (LaunchAgent), jadi tetap aktif saat jendela pilus ditutup dan dimulai lagi saat kamu login. Satu koneksi ini juga memberi akses ke semua mesin SSH-mu.

Kalau Mac tidur atau mati, tentu saja ponsel tidak bisa menjangkaunya.

## Yang bisa dilakukan

- Melihat daftar agent dan workspace, serta membuka layar agent yang sedang berjalan.
- Melihat agent yang aktif dan sesi terbaru, lalu melanjutkan (*continue*) atau mem-fork sesi.
- Membalas agent, meng-interrupt, atau menghentikannya. Untuk Claude dan Pi, kamu juga bisa memilih model dan effort.
- Meninjau diff, menambahkan catatan, lalu mengirimkannya ke agent.
- Melihat riwayat Git serta issue dan pull request GitHub.
- Membuka Mission, membuat tugas, dan mencari sesi.
- Menerima notifikasi saat agent membutuhkanmu.

## Notifikasi

iOS menangguhkan aplikasi tidak lama setelah kamu meninggalkannya, jadi aplikasi tidak bisa terus memantau agent di latar belakang. Mac yang melakukannya lewat service Remote Access. Untuk membangunkan ponsel, Mac mengirim notifikasi lewat Apple Push Notification service (APNs):

```
agent berhenti -> Luvus -> pilus-agent di Mac -> relay -> APNs -> ponsel
```

Ada dua jenis notifikasi: **`<Agent> is waiting your input`** saat agent berhenti untuk bertanya atau meminta izin, dan **`<Agent> finished`** saat gilirannya selesai. Notifikasi berisi judul sesi, nama pane, atau nama workspace. Mesin SSH yang tersimpan juga diperiksa: Mac mengeceknya tiap 10 detik saat ada agent yang bekerja, dan tiap 30 detik di waktu lain.

Ini satu-satunya bagian pilus yang mengirim data ke server saya, jadi berikut rinciannya:

- Relay berjalan sebagai Cloudflare Worker di `push.usepilus.dev`. Relay memegang kunci APNs karena kunci itu tidak boleh disimpan di komputer pengguna.
- Data yang dikirim mencakup token perangkat, judul dan isi notifikasi, serta penanda untuk mengganti notifikasi lama dari agent yang sama. Penanda itu memuat ID mesin dan pane. ID mesin SSH adalah nama yang kamu pilih sendiri. Relay tidak mencatat data ini, dan pilus juga tidak mencatat token, judul, atau isi notifikasi.
- Tanpa token perangkat yang terdaftar, tidak ada push yang dikirim. Selama belum ada ponsel yang mengaktifkan notifikasi, Mac tidak menjalankan pemeriksaan SSH tambahan untuk fitur ini.
- Matikan notifikasi di pengaturan aplikasi iOS atau cabut perangkat dari pilus untuk menghapus token. Selama host belum mengirim push, ponsel tetap memakai notifikasi lokal.

## Level akses

Setiap perangkat punya level akses. Kamu bisa menurunkan levelnya kapan saja dari Settings, atau mencabut aksesnya sepenuhnya.

| Level | Boleh |
| --- | --- |
| View | Membaca daftar, status, dan snapshot. |
| Control | Mengirim prompt, interrupt, fokus, dan review diff. |
| Operate | Semua yang bisa dilakukan Control, plus memulai tugas, agent, dan workspace. |

Operate setara dengan duduk langsung di depan mesin. Level ini hanya bisa diberikan lewat kode pairing baru, dengan **Allow this device to start work** dicentang. Di mesin SSH, Operate memberi otoritas penuh atas namamu. Berikan hanya jika memang perlu.
