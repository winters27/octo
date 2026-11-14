# Octo Family

Octo Family gives the people you share your music with accounts of their own. Each member
gets a library of their own in Navidrome for the songs they ask for, beside the music you
share with them.

## Where members' music lives

By default members' music is kept next to your library, in a folder named `.octo-family`
inside your music folder (the one Octo downloads to and Navidrome scans). Octo and Navidrome
already see that folder at the same path, so there is nothing to set up: no extra volume, no
extra folder on the host.

- Each member's songs are in `.octo-family/<username>`, which is that member's own library in
  Navidrome.
- A song two members want is kept once, in `.octo-family/.pool`, and linked into each
  member's folder.
- Your own library leaves the folder out. Navidrome skips folders whose names start with a
  dot, and when Family is turned on Octo also adds the line `.octo-family/` to the
  `.ndignore` file in your music folder, so the folder stays out even if Navidrome is set to
  scan such folders. Octo only adds that one line; anything else in the file stays as it was.
- Octo's own work on your library (covers, lyrics, explicit marks) leaves the folder out too.

When you turn Family on, Octo saves the folder it uses as `Family:MembersPath` in its
settings. If Octo later finds your music somewhere else, members' music does not move.

To keep members' music somewhere else, choose **Somewhere else** when you turn Family on and
give a full folder path. Mount that folder at the same path in both the octo and navidrome
containers (`docker-compose.yml` has a commented example). Octo checks that it can write
there when you turn Family on, and that Navidrome can see it when you add the first member.
Once members have music of their own, the folder stays where it is: Octo never moves
members' files.

An install that already keeps members' music in `/members` goes on using it.

### Music goes to

Each member has a **Music goes to** setting on the Family page, under **Adding music**:

- **Their own library** (the default for everyone): their songs land in their folder above.
- **Shared library**: what they add, the copies their plays keep and the requests you approve
  for them land in your shared library, the same place your own songs go. Their storage limit
  does not apply, since it measures their own folder. Their request limit and approvals still
  do.

A song another member already has is never downloaded a second time for a member whose music
goes to the shared library: Octo moves that file into the shared library, still one file,
and every member who had it keeps their stars and family playlist entries for it.

Such a member should see the shared library their songs land in. When the shared libraries
they see leave some out, the edit sheet says so, since songs they add may not show up for
them.

The Octo apps read the setting from the member's own Family state (`abilities.musicGoesTo`,
`Own` or `Shared`) and say where their songs go instead of showing a storage meter.

### Cloud and network storage

Family keeps a song two members want once on disk by linking it into each member's folder.
Some storage cannot keep links. Family works either way: it then stores a copy for each member
who wants the song, and the Family page says what to change.

- **rclone mounts:** add `--vfs-links` to the rclone mount that holds your music, then turn
  Family off and on so Octo checks again.
- **SMB or CIFS network shares:** add the `mfsymlinks` mount option to the share.
- **NFS:** links work, nothing to change.

## Removing a member

Removing a member takes away their own library and their sign-in on the Family page. What
happens to their songs is your choice: kept aside, added to the shared library, or deleted.
Shared music is never touched.

Their Navidrome account goes too, unless you turn on **Keep their account and password**.
Then the account stays with the password they chose, so their apps keep signing in, and it
keeps the shared libraries they saw. They lose their family settings and their open requests
are closed; from then on what they add goes to the shared library, as for anyone outside the
family. Octo refuses to keep an account that sees no shared library, or whose password it
cannot use (reset their password first).
