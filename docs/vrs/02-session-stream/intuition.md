# Session stream intuition

*For: protocol implementers and terminal embedders · Assumes: the session
runtime model · Covers: reconstructing one shared terminal correctly*

A screen baseline and live bytes are two halves of one ordered history. If the
boundary between them is approximate, bytes can be missing from both halves or
appear in both. The runtime therefore places an ordered marker in xterm's parser
queue, serializes after the marker, and holds later packets until that baseline
has been sent.

```text
parser input:  A B C | D E exit
                       ^ exact cut
client stream: GEOMETRY, SCREEN(A B C), DATA(D E), EXIT
```

Geometry is part of this history. A byte stream generated for 80 columns cannot
be parsed correctly into a 120-column grid and repaired later. Geometry changes
therefore occupy the same ordered stream before affected screen or data.

The machine attach surface deliberately does not invent a second protocol. It
forwards the existing frames on a descriptor that cannot be contaminated by
human terminal output or stderr. That keeps one ordering authority across local,
remote, interactive, and embedded clients.
