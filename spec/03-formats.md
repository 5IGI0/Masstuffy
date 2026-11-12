# 3. Formats

## 3.1 Record WARC (sérialisation)

Masstuffy écrit et lit le **WARC 1.1** :

```
WARC/1.1\r\n
<Clé>: <valeur>\r\n
...
Content-Length: <taille du corps>\r\n
\r\n
<corps : Content-Length octets>
\r\n\r\n
```

Règles :

- La première ligne doit être exactement `WARC/1.1\r\n`. Les autres versions sont refusées.
- Un en-tête est découpé au **premier** `": "`. Une ligne sans `": "` est une erreur.
- `Content-Length` est **toujours recalculé** à l'écriture : il n'est jamais repris de l'entrée.
- `WARC-Type`, `WARC-Record-ID` et `WARC-Date` sont **uniques** : une seconde occurrence remplace la première. Les autres en-têtes peuvent apparaître plusieurs fois.
- Après le corps, on attend exactement `\r\n\r\n`.
- Si un record arrive sans `WARC-Record-ID` ou sans `WARC-Date`, on génère `<urn:uuid:v4>` et la date actuelle (RFC 3339).

[LEGACY] Les en-têtes sont stockés dans une table de hachage. **Leur ordre n'est pas conservé**, et la sérialisation n'est donc pas identique octet par octet à l'entrée. [RECO] Conserver l'ordre, pour pouvoir garantir l'intégrité (et des digests stables).

## 3.2 Stockage dans un segment

### Collection sans compression

Les records sérialisés sont simplement mis bout à bout. On obtient un fichier `.warc` standard.

```
| record 1 | record 2 | record 3 | ...
^offset1   ^offset2
raw_size = taille sérialisée du record
```

### Collection compressée (zstd + dictionnaire)

Chaque record est compressé **seul** dans une **frame zstd complète** avec le dictionnaire de la collection et le niveau `compression_level`. Les frames sont mises bout à bout :

```
| frame zstd(record 1) | frame zstd(record 2) | ...
^offset1               ^offset2
raw_size = taille de la frame compressée
```

Conséquences :

- **Accès direct** : on décompresse une frame à partir de son offset, sans rien lire avant.
- **Compatibilité** : une suite de frames zstd forme un flux zstd valide. Le segment entier se décompresse avec `zstd -d -D <dict>` et donne un `.warc` valide.
- **Délégation de la décompression** : un client peut recevoir les `raw_size` octets bruts avec le `dict_id`, récupérer le dictionnaire une fois, puis décompresser lui-même.
- **Délégation de la compression** : un client qui a le dictionnaire peut envoyer des frames déjà compressées. Le serveur se contente de les ajouter et de les indexer.

Chaque frame contient le `dict_id` dans son en-tête (fonctionnement standard de zstd). Une frame est donc auto-descriptive sur le dictionnaire qu'il lui faut.

## 3.3 Dictionnaire zstd

C'est un dictionnaire zstd standard (format « formatted dictionary ») :

| Octets | Contenu |
|---|---|
| 0–3 | Magic `0xEC30A437` (little-endian : `37 A4 30 EC`) |
| 4–7 | `dict_id`, u32 little-endian |
| 8– | Tables d'entropie et contenu |

Allocation des ids générés :

- On garde l'id choisi par l'entraîneur zstd s'il est libre.
- Sinon, on tire un id au hasard dans **[32768, 2³¹)** jusqu'à en trouver un libre, puis on le réécrit dans les octets 4 à 7. Ces bornes évitent les plages réservées par zstd (0 à 32767 et ≥ 2³¹).

Paramètres d'entraînement par défaut : jusqu'à **100 000 échantillons**, dictionnaire de **5 Mo** au maximum.

## 3.4 Ligne d'index (CDX Masstuffy)

C'est un format propre à Masstuffy : une ligne par record, **7 champs séparés par une espace**, terminée par `\n`. `-` marque un champ absent.

```
<url> <type> <record_id> <date> <filename> <offset> <raw_size>
```

| # | Champ | Format |
|---|---|---|
| 1 | `url` | `WARC-Target-URI` brut, ou `-` |
| 2 | `type` | `WARC-Type` |
| 3 | `record_id` | `WARC-Record-ID` **sans** `<>` |
| 4 | `date` | `YYYYmmddHHMMSS` (UTC) |
| 5 | `filename` | nom du segment, relatif au répertoire de la collection |
| 6 | `offset` | décimal |
| 7 | `raw_size` | décimal, taille stockée (compressée si besoin) |

Exemple :

```
https://example.com/ response urn:uuid:0f3e… 20250503114301 records.1.1234567890.warc.zstd 48213 1532
```

Il n'y a ni en-tête de fichier ni tri : l'ordre des lignes est l'ordre d'insertion.

Ce même format sert aussi d'**enveloppe d'import brut**. Le client envoie une suite de `ligne CDX\n` suivie chacune de `raw_size` octets de frame. Les champs `filename` et `offset` envoyés sont ignorés, puis réécrits par le serveur.

[LEGACY] Limites :

- une URL qui contient une espace casse la ligne (pas d'échappement) ;
- le format n'est pas compatible avec le CDX standard (pas de SURT en tête, pas de ligne d'en-tête ` CDX …`) ;
- la ligne ne contient ni `dict_id` ni digest : on déduit la génération du nom du segment.

## 3.5 Ligne d'index secondaire (base)

Voici, conceptuellement, ce qu'une entrée de l'index secondaire doit contenir. C'est indépendant du moteur de stockage.

| Champ | Notes |
|---|---|
| `id` | clé technique (séquence) |
| `flags` | bit 0 = ACTIVE. Les autres bits sont libres. |
| `date` | timestamp |
| `identifier` | `record_id`. Indexé, **non unique**. |
| `collection` | **uuid** de la collection |
| `filename`, `offset`, `raw_size` | localisateur |
| `type` | `WARC-Type` |
| `uri` | URL brute. Indexée (égalité). |
| `massaged_url` | URL normalisée. Indexée (ordonnée, pour les préfixes et regex). |
| `dict_type`, `dict_id` | génération |

L'échantillonnage pour l'entraînement du dictionnaire suit un ordre **pseudo-aléatoire déterministe** (un hachage de `id`) : deux générations successives sur les mêmes données donnent le même échantillon.
