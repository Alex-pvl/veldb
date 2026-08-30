# ClickBench на veldb

- строк: **3000000**
- в памяти: **2.37 ГиБ** (847 байт/строку)
- загрузка: **5.1 с** (585 тыс. строк/с)
- прогонов на запрос: 3, в таблице медиана
- выполнено: **42/43**, суммарно **5.1 с**

1 запрос(ов) не выполнились — они перечислены ниже со статусом.

| # | мс | строк | запрос |
|---:|---:|---:|:---|
| 1 | 6.0 | 1 | `SELECT COUNT(*) FROM hits` |
| 2 | 14.6 | 1 | `SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0` |
| 3 | 12.1 | 1 | `SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits` |
| 4 | 11.1 | 1 | `SELECT AVG(UserID) FROM hits` |
| 5 | 10.3 | 1 | `SELECT COUNT(DISTINCT UserID) FROM hits` |
| 6 | 67.6 | 1 | `SELECT COUNT(DISTINCT SearchPhrase) FROM hits` |
| 7 | 8.8 | 1 | `SELECT MIN(EventDate), MAX(EventDate) FROM hits` |
| 8 | 29.1 | 50 | `SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY AdvEngineID ORDER BY COUNT(*) DESC` |
| 9 | 70.6 | 10 | `SELECT RegionID, COUNT(DISTINCT UserID) AS u FROM hits GROUP BY RegionID ORDER BY u DESC LIMIT 10` |
| 10 | 71.1 | 10 | `SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), COUNT(DISTINCT UserID) FROM hits GROUP…` |
| 11 | 20.0 | 0 | `SELECT MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePho…` |
| 12 | 19.9 | 0 | `SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP…` |
| 13 | 144.6 | 4 | `SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LI…` |
| 14 | 162.0 | 4 | `SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDE…` |
| 15 | 157.1 | 10 | `SELECT SearchEngineID, SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID,…` |
| 16 | 40.0 | 10 | `SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10` |
| 17 | 301.9 | 10 | `SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10` |
| 18 | 287.1 | 10 | `SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase LIMIT 10` |
| 19 | 515.9 | 10 | `SELECT UserID, EXTRACT(MINUTE FROM EventTime) AS m, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, m, Searc…` |
| 20 | 4.4 | 0 | `SELECT UserID FROM hits WHERE UserID = 435090932899640449` |
| 21 | 175.2 | 1 | `SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'` |
| 22 | 259.9 | 4 | `SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits WHERE URL LIKE '%google%' AND SearchPhrase <> '' GROUP …` |
| 23 | 407.6 | 0 | `SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, COUNT(DISTINCT UserID) FROM hits WHERE Title LIKE '%…` |
| 24 | 178.3 | 10 | `SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10` |
| 25 | 79.5 | 10 | `SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10` |
| 26 | 149.2 | 10 | `SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY SearchPhrase LIMIT 10` |
| 27 | 123.3 | 10 | `SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime, SearchPhrase LIMIT 10` |
| 28 | 130.1 | 1 | `SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits WHERE URL <> '' GROUP BY CounterID HAVING COU…` |
| 29 | — (функция 'REGEXP_REPLACE' не поддерживается) | 0 | `SELECT REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1') AS k, AVG(length(Referer)) AS l, COUNT…` |
| 30 | 151.1 | 1 | `SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), SUM(ResolutionWidth + 2), SUM(ResolutionWidth + 3), SUM…` |
| 31 | 85.5 | 10 | `SELECT SearchEngineID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhr…` |
| 32 | 276.1 | 10 | `SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> …` |
| 33 | 262.1 | 10 | `SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits GROUP BY WatchID, Clie…` |
| 34 | 83.1 | 10 | `SELECT URL, COUNT(*) AS c FROM hits GROUP BY URL ORDER BY c DESC LIMIT 10` |
| 35 | 91.8 | 10 | `SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL ORDER BY c DESC LIMIT 10` |
| 36 | 45.7 | 10 | `SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c FROM hits GROUP BY ClientIP, ClientIP…` |
| 37 | 107.3 | 10 | `SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= 15887 AND EventDate <= 15917…` |
| 38 | 158.0 | 10 | `SELECT Title, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= 15887 AND EventDate <= 159…` |
| 39 | 52.0 | 10 | `SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= 15887 AND EventDate <= 15917…` |
| 40 | 221.7 | 10 | `SELECT TraficSourceID, SearchEngineID, AdvEngineID, CASE WHEN SearchEngineID = 0 AND AdvEngineID = 0 THEN Refe…` |
| 41 | 39.6 | 0 | `SELECT URLHash, EventDate, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= 15887 AND Eve…` |
| 42 | 36.8 | 0 | `SELECT WindowClientWidth, WindowClientHeight, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDa…` |
| 43 | 32.6 | 10 | `SELECT date_trunc('minute', EventTime) AS M, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDat…` |
