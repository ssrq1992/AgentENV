CREATE TABLE IF NOT EXISTS agentenv_adapter_operations (
 id text PRIMARY KEY, tenant text NOT NULL, instance text NOT NULL UNIQUE,
 digest text NOT NULL, request jsonb NOT NULL, resources jsonb NOT NULL,
 result jsonb NOT NULL DEFAULT '{}', state text NOT NULL DEFAULT 'Pending',
 reason text NOT NULL DEFAULT '', stopping boolean NOT NULL DEFAULT false,
 sandbox_id text NOT NULL DEFAULT '', native jsonb, updated timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS agentenv_adapter_reservations (
 operation text PRIMARY KEY, node text NOT NULL, node_uid text NOT NULL, boot_id text NOT NULL,
 cpu bigint NOT NULL CHECK(cpu>0), memory_mib bigint NOT NULL CHECK(memory_mib>0),
 expires timestamptz NOT NULL, state text NOT NULL DEFAULT 'Reserved'
);
CREATE INDEX IF NOT EXISTS agentenv_adapter_capacity ON agentenv_adapter_reservations(node,state);
CREATE TABLE IF NOT EXISTS agentenv_adapter_volumes (
 namespace text NOT NULL, name text NOT NULL, tenant text NOT NULL,
 digest text NOT NULL, request jsonb NOT NULL, uid text NOT NULL DEFAULT '',
 initialize_ext4 boolean NOT NULL, initialization_owner text NOT NULL DEFAULT '',
 PRIMARY KEY(namespace,name)
);

ALTER TABLE agentenv_adapter_operations ADD COLUMN IF NOT EXISTS timings jsonb NOT NULL DEFAULT '{}';
