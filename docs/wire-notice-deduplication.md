# Wire notice deduplication

`MessageStore::send_wire_once_awaiting_daemon` atomically checks a caller-supplied
message ID and, if absent, stores the message, its initial receipts, and its
daemon marker together. If the ID is already retained, it returns `None` without
resetting receipts or creating another marker. A missing ID is refused. This is
an additive store operation: it changes neither existing `send_wire` behavior
nor the schema or on-disk format. A backend without atomic support refuses it.

The guarantee lasts only while the message is retained. Once normal Wire pruning
removes a terminal message, the same ID can be inserted again. A permission
prompt that remains open beyond that retention period may therefore produce a
new notice, which acts as a reminder.
