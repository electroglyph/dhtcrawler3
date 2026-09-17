# Secrets

Database passwords for `deploy/docker-compose.yml` live here, one file per
secret. They are ignored by git and excluded from the Docker build context.

Create them with:

```sh
scripts/gen-secrets.sh
```

Files: `pg_superuser_password`, `dc3_owner_password`, `dc3_crawler_password`,
`dc3_indexer_password`, `dc3_web_password`.
