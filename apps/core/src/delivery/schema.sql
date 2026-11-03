CREATE TABLE orders(id TEXT PRIMARY KEY,network TEXT,intent TEXT NOT NULL,role TEXT NOT NULL,body TEXT NOT NULL);
CREATE INDEX orders_intent ON orders(network,intent,role);
CREATE TABLE charity_orders(id TEXT PRIMARY KEY,network TEXT,intent TEXT NOT NULL,body TEXT NOT NULL);
CREATE INDEX charity_intent ON charity_orders(network,intent);
CREATE TABLE objects(order_id TEXT NOT NULL,idx INTEGER NOT NULL,kind TEXT NOT NULL,hash TEXT NOT NULL,size INTEGER NOT NULL,committed INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(order_id,idx,kind));
CREATE TABLE private_values(id TEXT PRIMARY KEY,body BLOB NOT NULL);
CREATE TABLE proof_jobs(digest TEXT PRIMARY KEY,path TEXT NOT NULL,size INTEGER NOT NULL);
CREATE TABLE gas_budgets(chain INTEGER NOT NULL,wallet TEXT NOT NULL,budget_group TEXT NOT NULL,cap TEXT NOT NULL,PRIMARY KEY(chain,wallet,budget_group));
CREATE TABLE transactions(id TEXT PRIMARY KEY,chain INTEGER NOT NULL,wallet TEXT NOT NULL,nonce INTEGER NOT NULL,intent TEXT NOT NULL,raw TEXT,hash TEXT,phase TEXT NOT NULL,block_number INTEGER,block_hash TEXT,error TEXT,checked_at INTEGER NOT NULL DEFAULT 0,UNIQUE(chain,wallet,nonce));
