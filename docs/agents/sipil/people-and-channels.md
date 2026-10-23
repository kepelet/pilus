# Sipil: people, channels and mentions

Read this before you change contacts, direct chats, channels, or how an agent
is brought into a conversation.

## Contacts and direct messages

- Adding a contact is mutual and done by hand: each side adds the other.
  Contacts are renamed in the Sipil tab (`Directory::rename`). The name belongs
  to the user and never comes from the peer.
- A stranger's `chat.send` is refused without being read and is remembered as a
  *request* that the person can accept.
- A message is stored on the sender's side only once the receiver's pilus has
  taken it, so after a refusal the composer still holds it for a retry.
- Messages arrive as `Event`s. `gui/sipil.rs::take_events` hands them to the
  main window as toasts, and as system notifications while the window is not
  in front. Nothing is delivered while the window is closed: the node lives in
  the window, not in `pilus-agent`.

## Channels

Channels (the `channels` document, `pilus_client::sipil::Channels`) are named
rooms of contacts with no server.

- The author's pilus fans `chan.send` out to every member that is a contact, in
  parallel, so one absent member holds nobody up. Each member keeps its own
  copy under the thread `#<id>`.
- A pilus learns about a channel from the first message that names it.
- Messages are sent only to contacts and accepted only from contacts. A message
  from a stranger, or for a channel this pilus left, is dropped
  (`Learned::Left`).
- When some members miss a message, the message says who. There is no
  store-and-forward.

### Ownership and membership

- A channel's owner is the device that made it (`Channel::owner`, carried in
  every `chan.send`). Only the owner can invite a contact
  (`Local::invite_to_channel`) or remove a member (`remove_from_channel`).
  Anyone else is refused with `not_owner`.
- An invite or a removal is a line to the members that carries `added` or
  `removed` device IDs next to the usual member list.
- A pilus that has the channel trusts only the owner's lists and changes. It
  refuses a message from anyone not in its list (`not_member`), so a removed
  member can neither read nor speak. The removed member is told and forgets the
  channel. A member who was away finds out when every member refuses its next
  message.
- Removals are kept per channel (`Channels::remove_members`, `removed` in the
  `channels` document), so an older list does not bring the person back. Only
  an `added` does.

### The channel panel

The info button on a channel's page opens `channel_panel`, which lists the
members with their device IDs and shows who owns the channel. The owner also
sees Remove (two clicks) and the contacts who can be invited. Anyone can leave.
The page closes itself when the channel is gone.

## Mentioning an agent

A daemon's agent is not shared with a contact. No call hands anyone a session
of it, and `health` lists no agents. An agent takes part only in a channel its
person invited it into.

- An agent is invited from the channel panel's "Your agents" list
  (`Channels::set_agent`). The invitation is kept in the `channels` document on
  that device only and is never sent. The invite asks for confirmation once,
  because every member can then run a model with file and shell tools on that
  machine, using its API keys. The agent announces joining or leaving in the
  channel.
- Mentioning an invited agent (`@Researcher`, matched case-insensitively
  against the connected daemons' agent names by `mention::mentioned`) sends the
  line as usual and hands the conversation to that agent (`mention::answer`):
  - its daemon gets the lines it has not seen, with the speaker of each, in one
    session per conversation (the cursor lives in the chat store);
  - its reply goes back under its name (`as`), to everyone.
- Mentioning an agent that was not invited does nothing, for the person and for
  every member.
- For a line a member sends, `PeerService::invite_agents` turns the mention into
  `Event::Mentioned`, which `gui/sipil_social.rs::push_event` answers on the
  spot. For the person's own line, `bring_in_agents` does the same.
- A line an agent said (`as`) never starts one of this pilus's agents, whatever
  it says.
- A direct chat between two people is never handed to an agent. No agent reads
  or speaks in it, and mentioning one there does nothing from either side:
  `bring_in_agents`, `answer_invited` and `answer_mention` all refuse a
  `Conversation::Direct`.
- Leaving a channel forgets its invitations.
