# Operations

How the service ships, recovers and rolls back.

## Deploy

Run `make deploy` from a clean checkout.

## Retries

Downloads are retried five times with exponential backoff.
The first wait is 250 ms and it doubles after each attempt.

## Rollback

Redeploy the previous release tag.
