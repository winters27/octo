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

A member's **Music goes to** ability sets where their songs land. **Their own library** (the
default) is the folder above. **Shared library** sends what they add, and their approved
requests, to the library everyone plays, the same place a non-member's songs go. Their storage
limit does not apply then, since their own folder is no longer where new songs land; their
request limit and approvals still do. A member whose music goes to the shared library needs to
see that library: when their shared-library choice leaves some of them out, the edit sheet warns,
since songs they add may not show up for them.

An install that already keeps members' music in `/members` goes on using it.

### Cloud and network storage

Family keeps a song two members want once on disk by linking it into each member's folder.
Some storage cannot keep links. Family works either way: it then stores a copy for each member
who wants the song, and the Family page says what to change.

- **rclone mounts:** add `--vfs-links` to the rclone mount that holds your music, then turn
  Family off and on so Octo checks again.
- **SMB or CIFS network shares:** add the `mfsymlinks` mount option to the share.
- **NFS:** links work, nothing to change.

## Leaving Family

Removing a member deletes their Navidrome account and their own library, unless you choose
**Keep their account and password**. Then their Navidrome account stays with the password they
chose, so their apps keep signing in; they lose their family settings and their requests are
closed. They keep the shared libraries they saw, and Octo refuses if they see none, or if they
have no password Octo can use (reset it first). Their own library is deleted in every case; what
happens to their files is your choice: kept aside, added to the shared library, or deleted.
Once they are out of the family, what they add goes to the shared library, as for anyone
outside it.
