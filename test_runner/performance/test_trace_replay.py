from contextlib import closing

from fixtures.neon_fixtures import NeonEnvBuilder, ReplayBin


def test_trace_replay(neon_env_builder: NeonEnvBuilder, replay_bin: ReplayBin):
    neon_env_builder.num_safekeepers = 1
    env = neon_env_builder.init_start()

    tenant, _ = env.neon_cli.create_tenant(
        conf={
            "trace_read_requests": "true",
        }
    )
    env.neon_cli.create_timeline("test_trace_replay", tenant_id=tenant)
    pg = env.postgres.create_start("test_trace_replay", "main", tenant)

    pg.safe_psql("select 1;")
    pg.safe_psql("select 1;")
    pg.safe_psql("select 1;")
    pg.safe_psql("select 1;")

    with closing(pg.connect()) as conn:
        with conn.cursor() as cur:
            cur.execute("create table t (i integer);")
            cur.execute(f"insert into t values (generate_series(1,{10000}));")
            cur.execute("select count(*) from t;")

    # Stop pg so we drop the connection and flush the traces
    pg.stop()

    # TODO turn off tracing now?

    # trace_path = env.repo_dir / "traces" / str(tenant) / str(timeline) / str(timeline)
    # assert trace_path.exists()

    print("replaying")
    ps_connstr = env.pageserver.connstr()
    # ps_connstr = "host=localhost port=15004 dbname=postgres user=neon_admin"
    output = replay_bin.replay_all(ps_connstr)
    print(output)
