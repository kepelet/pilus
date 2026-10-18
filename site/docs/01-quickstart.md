---
title: Memulai
description: Dari pilus yang baru dipasang sampai agent pertama yang berjalan.
sidebar:
  icon: rocket
---

## 1. Buka pilus

Saat pertama dibuka, pilus mencari socket klien Luvus. Kalau daemon belum berjalan, pilus
menyalakannya. Di peluncuran pertama pilus juga menyesuaikan beberapa kunci konfigurasi daemon
(pane tanpa jarak, tanpa baris judul, tema `noir`) supaya tampilannya sesuai rancangan.

## 2. Buka workspace

Tekan `cmd + shift + o` (atau tombol `+` di sidebar) untuk memilih folder. Dropdown host di atas dialog
memilih mesin yang dijelajahi: Local atau mesin SSH. **Open with new worktree** membuat
Git worktree baru dengan nama branch otomatis.

## 3. Jalankan agent

`cmd + shift + n` membuka composer Home: tulis prompt, pilih agent dan tempat menjalankannya.
Agent yang berjalan muncul di dock **AGENTS**, dikelompokkan per mesin. Klik satu untuk
memfokuskan pane-nya.

## 4. Pantau dan tinggalkan

Tutup jendela kapan saja: Luvus tetap berjalan. Saat agent berhenti menunggu jawabanmu atau
selesai, pilus mengirim notifikasi dengan suara dan lencana. Buka lagi dan semuanya masih di tempatnya.

## Jalan pintas

| Tombol | Aksi |
| --- | --- |
| `⌘⇧P` | Command palette |
| `⌘⇧O` | Buka workspace |
| `⌘⇧N` | Composer Home (agent baru) |
| `⌘B` | Tampilkan/sembunyikan sidebar kanan |
| `⌘1`–`⌘9` | Pilih tab ke-N |
| `⌘⇧.` | Settings |

## Selanjutnya

- [Hubungkan ke mesin remote](/guides/connect-remote)
- [Antarmuka](/features/ui)
- [Aplikasi iOS](/features/ios)
