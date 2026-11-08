-- Client IP of the proxied caller, resolved behind a trusted reverse proxy
-- via X-Forwarded-For (see HYDRA_TRUSTED_PROXIES). NULL for rows written
-- before the column existed and for requests served without peer info.
ALTER TABLE request_logs ADD COLUMN client_ip TEXT;
