# Secrets

Database passwords for `deploy/docker-compose.yml` live here, one file per
secret. They are ignored by git and excluded from the Docker build context.

Create them with:

```sh
scripts/gen-secrets.sh
```

Files: `pg_superuser_password`, `dc4_owner_password`, `dc4_crawler_password`,
`dc4_indexer_password`, `dc4_web_password`.
