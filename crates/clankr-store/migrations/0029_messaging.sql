-- Request/reply (CL-30-05): a request names the queue its reply goes to ("reply:<id>").
alter table mailbox_messages add column reply_to text;
