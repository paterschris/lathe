-- name: daily
SELECT toStartOfDay(timestamp) AS day, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY
  AND blob6 != 'registry'
GROUP BY day
ORDER BY day;

-- name: weekly
SELECT toStartOfWeek(timestamp) AS week, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY
  AND blob6 != 'registry'
GROUP BY week
ORDER BY week;

-- name: versions
SELECT blob1 AS version, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '7' DAY
  AND blob6 != 'registry'
GROUP BY version
ORDER BY active_installs DESC;

-- name: channels
SELECT blob2 AS channel, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '7' DAY
  AND blob6 != 'registry'
GROUP BY channel
ORDER BY active_installs DESC;

-- name: platforms
SELECT blob3 AS os, blob4 AS arch, count(DISTINCT index1) AS active_installs
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '7' DAY
  AND blob6 != 'registry'
GROUP BY os, arch
ORDER BY active_installs DESC;

-- name: sampling
SELECT max(_sample_interval) AS largest_sample_interval
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY;

-- name: registry_daily
SELECT toStartOfDay(timestamp) AS day, count(DISTINCT index1) AS installs_fetching_registry, count() AS registry_fetches
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY
  AND blob6 = 'registry'
GROUP BY day
ORDER BY day;

-- name: registry_installs
SELECT substring(index1, 1, 8) AS install, blob5 AS country, min(timestamp) AS first_fetch, max(timestamp) AS last_fetch, count() AS fetches
FROM lathe_pings
WHERE timestamp >= NOW() - INTERVAL '30' DAY
  AND blob6 = 'registry'
GROUP BY index1, blob5
ORDER BY first_fetch;
