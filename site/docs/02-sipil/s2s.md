---
title: sipil to sipil
description: Agent yang bertanya ke agent lain, dan pilus yang mengobrol dengan pilus.
sidebar:
  hidden: true
---

"sipil" di sini bisa dua hal: agent, atau pilus yang menjalankan jendela kamu. Keduanya bicara dengan
protokol yang sama lewat iroh, jadi sama-sama bisa menambah kontak dan saling kirim pesan.

## Agent sendiri

Setiap agent adalah file. Taruh `~/.sipil/agents/reviewer.md`:

```markdown
---
name: Reviewer
summary: Mereview perubahan dan bilang apa yang salah.
tools: [read, bash]
---
Baca diff, jalankan test, lalu jawab dengan apa yang salah. File:baris dulu, baru yang sudah benar.
```

- Simpan filenya dan perubahan berlaku di giliran berikutnya. Tidak perlu restart. File yang rusak
  dilaporkan di log dan dashboard lalu dilewati, jadi agent yang sedang jalan tidak ikut mati karena
  salah ketik.
- Tiap file menjadi sipil sendiri di dalam satu proses: device ID, percakapan, dan memori sendiri di
  `~/.sipil/primaries/<id>/`, serta satu baris sendiri di sidebar pilus. Tidak ada subagent.
- Agent baru meminta semua pilus yang sudah diizinkan untuk menambahkannya, sekali. Konfirmasi di pilus
  dan barisnya muncul. Untuk pilus yang sedang mati saat itu, pakai
  `npm run connect -- --device-id <id pilus>`.
- Mengganti nama file berarti agent baru. Ganti `name:` kalau mau ganti nama.

Chief of staff juga bisa menulis file ini untukmu (`create_agent`, `update_agent`, `delete_agent`).
Tidak ada yang ditimpa diam-diam, dan `sipil.md` sengaja tidak boleh disentuh.

## Agent bertanya ke agent

Setiap agent diberi tahu siapa saja agent lain dan untuk apa, dari `summary` mereka. Kalau
pertanyaannya di bidang orang lain, ia memakai `ask_agent`.

- Pertanyaan masuk ke percakapan agent itu sebagai barisnya, dan jawabannya kembali dengan cara yang
  sama.
- Tidak ada yang menunggu. Dua agent yang saling bertanya tidak bisa deadlock.
- Satu pasang boleh bertanya enam kali dalam sepuluh menit. Setelah itu mereka disuruh menanyakannya
  ke kamu.
- Pertanyaan ditulis dulu sebelum dikirim. Kalau sipil restart di tengah, penantian dilanjutkan dan
  jawabannya datang sekali.
- Keduanya diberi tahu bahwa pesan dari agent lain adalah kata-kata agent itu, bukan kata-katamu.

## Coding agent bicara ke sipil

Coding agent bisa menulis ke chief langsung dari terminalnya:

```sh
node /path/to/sipil/src/main.ts tell "Selesai: basic auth masuk, test lulus."
node /path/to/sipil/src/main.ts tell --agent ops "Staging-nya host yang mana?"
```

Pesannya sampai di percakapan agent itu, dengan nama pengirimnya. Antar coding agent, lewatnya selalu
chief: ia membaca satu dengan `pilus_agent_read`, meneruskan yang penting ke yang lain dengan
`pilus_agent_send`, dan diberi tahu kalau yang kedua selesai. Cukup minta dengan kalimat biasa.

## Pilus ke pilus

- **Kontak.** Menambah kontak dilakukan dua arah dan manual: kamu menambahkan dia, dia menambahkan
  kamu. Nama kontak milikmu, tidak pernah dikirim oleh dia. Pesan dari orang asing ditolak tanpa
  dibaca dan diingat sebagai permintaan yang bisa kamu terima.
- **Channel.** Ruang bernama tanpa server. Penulis mengirim ke tiap anggota yang jadi kontaknya, dan
  tiap anggota menyimpan salinannya sendiri. Kalau ada yang sedang mati, pesannya tidak menunggu dan
  pilus bilang siapa yang terlewat.
- **Pemilik.** Yang membuat channel adalah pemiliknya. Hanya pemilik yang bisa mengundang atau
  mengeluarkan anggota, dan daftar dari orang lain tidak dipercaya. Anggota yang dikeluarkan tidak bisa
  lagi membaca maupun menulis.
- **Pesan langsung.** Dua orang saja. Agent tidak pernah membaca atau menjawab di sini, siapa pun yang
  menyebutnya.

## Menyebut agent

Sebut `@Reviewer` di channel dan agent itu ikut menjawab, atas nama dirinya, ke semua anggota. Ada
aturannya:

- Agent hanya menjawab di channel yang ia diundang masuk oleh pemiliknya, lewat daftar "Your agents" di
  panel channel. Undangannya hanya tersimpan di perangkat itu dan tidak dikirim ke mana pun.
- Dari sudut pandang anggota lain, menyebut agent yang tidak diundang tidak melakukan apa-apa.
- Mengundang agent berarti semua anggota bisa menjalankan model dengan akses file dan shell di mesin
  itu, memakai API key-nya. Karena itu pilus meminta konfirmasi sekali.
- Kata-kata agent tidak pernah memicu agent lain di pilus ini.

Pesan tidak diterima selama jendela pilus ditutup. Node sipil hidup di dalam jendela, bukan di
`pilus-agent`.
