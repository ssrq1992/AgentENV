-- Apply as database owner after ssfctl migrate. These are NOLOGIN group roles;
-- grant them to separately provisioned authenticated service login roles.
CREATE ROLE ssf_guard_reader NOLOGIN;
GRANT USAGE ON SCHEMA public TO ssf_guard_reader;
GRANT SELECT ON ssf_permits, ssf_selection_permits TO ssf_guard_reader;

CREATE ROLE ssf_gateway_reader NOLOGIN;
GRANT USAGE ON SCHEMA public TO ssf_gateway_reader;
GRANT SELECT ON ssf_bindings, ssf_attachments TO ssf_gateway_reader;

CREATE ROLE ssf_consumer NOLOGIN;
GRANT USAGE ON SCHEMA public TO ssf_consumer;
GRANT SELECT, INSERT, UPDATE ON ssf_volumes, ssf_operations, ssf_bindings,
    ssf_steps, ssf_attachments, ssf_permits, ssf_selection_permits TO ssf_consumer;
GRANT SELECT ON ssf_meta TO ssf_consumer;
