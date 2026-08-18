CREATE TABLE messages (
    id   INTEGER PRIMARY KEY,
    data message_data NOT NULL
) STRICT;

CREATE INDEX messages_by_tag ON messages(union_tag(data));
