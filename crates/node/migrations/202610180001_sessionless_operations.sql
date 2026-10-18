CREATE TABLE operations_v3 (
  network_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  target_node TEXT NOT NULL,
  session_id TEXT,
  request TEXT NOT NULL,
  state TEXT NOT NULL,
  control INTEGER NOT NULL DEFAULT 0,
  result TEXT NOT NULL,
  PRIMARY KEY(network_id,operation_id)
);
INSERT INTO operations_v3 SELECT * FROM operations;
CREATE TABLE resource_reservations_v3 (
  network_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  target_node TEXT NOT NULL,
  resource TEXT NOT NULL,
  PRIMARY KEY(network_id,operation_id,resource),
  FOREIGN KEY(network_id,operation_id) REFERENCES operations_v3(network_id,operation_id)
);
INSERT INTO resource_reservations_v3 SELECT * FROM resource_reservations;
DROP TABLE resource_reservations;
DROP TABLE operations;
ALTER TABLE operations_v3 RENAME TO operations;
ALTER TABLE resource_reservations_v3 RENAME TO resource_reservations;
CREATE INDEX operations_session ON operations(network_id,target_node,session_id,state);
PRAGMA user_version = 3;
