-- Minimal subset of the opencode 2.x schema used by petsitter, with synthetic rows.
CREATE TABLE session_v2 (
  id text PRIMARY KEY, project_id text NOT NULL, parent_id text, slug text NOT NULL,
  directory text NOT NULL, title text, version text NOT NULL, model text,
  time_created integer NOT NULL, time_updated integer NOT NULL,
  time_busy integer, time_idle integer, time_archived integer
);
CREATE TABLE session_message (
  id text PRIMARY KEY, session_id text NOT NULL, type text NOT NULL, seq integer NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL
);
CREATE TABLE session_part (
  message_id text NOT NULL, session_id text NOT NULL, ordinal integer NOT NULL, type text NOT NULL,
  call_id text, time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL,
  PRIMARY KEY (message_id, ordinal)
);

INSERT INTO session_v2 VALUES
  ('ses_a', 'prj', NULL, 'brave-fox', '/home/dev/lab/demo', 'Refactor the lexer', '2.4.1',
   '{"providerID":"anthropic","modelID":"claude-sonnet-5-5"}',
   1767261000000, 1767261690000, 1767261650000, 1767261600000, NULL),
  ('ses_b', 'prj', NULL, 'calm-otter', '/home/dev/lab/other', '', '2.4.1', NULL,
   1767250000000, 1767260000000, 1767259000000, 1767259500000, NULL),
  ('ses_c', 'prj', 'ses_a', 'child', '/home/dev/lab/demo', 'subagent', '2.4.1', NULL,
   1767261000000, 1767261690000, 1767261650000, NULL, NULL),
  ('ses_d', 'prj', NULL, 'away', '/tmp/elsewhere', 'out of scope', '2.4.1', NULL,
   1767261000000, 1767261690000, NULL, NULL, NULL);

INSERT INTO session_message VALUES
  ('msg_1', 'ses_a', 'user', 1, 1767261600000, 1767261600000, '{"role":"user"}'),
  ('msg_2', 'ses_a', 'assistant', 2, 1767261605000, 1767261690000,
   '{"role":"assistant","modelID":"claude-sonnet-5-5","tokens":{"input":4000,"output":200,"cache":{"read":9000,"write":0}}}');

INSERT INTO session_part VALUES
  ('msg_1', 'ses_a', 0, 'text', NULL, 1767261600000, 1767261600000,
   '{"type":"text","text":"Refactor the lexer\nplease"}'),
  ('msg_2', 'ses_a', 0, 'tool', 'call_1', 1767261610000, 1767261615000,
   '{"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath":"/home/dev/lab/demo/src/lexer.rs"}}}'),
  ('msg_2', 'ses_a', 1, 'tool', 'call_2', 1767261620000, 1767261620000,
   '{"type":"tool","tool":"bash","state":{"status":"running","input":{"command":"cargo check"}}}');
