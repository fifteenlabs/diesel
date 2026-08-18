CREATE TYPE telegram_t AS STRUCT(chat_id INT, text TEXT);
CREATE TYPE slack_t    AS STRUCT(channel_id_hash INT, text TEXT);
CREATE TYPE message_data AS UNION(telegram telegram_t, slack slack_t);
