from article import Article

def headers_to_articles(headers):
    articles = []

    for number, header in headers:
        article = Article(
            number= number,
            subject= header["subject"],
            author= header["from"],
            date= header["date"],
            message_id= header["message-id"],
            references= header["references"],
            bytes= int(header[":bytes" if ":bytes" in header else "bytes"]),
            lines= int(header[":lines" if ":lines" in header else "lines"])
        )

        articles.append(article)

    return articles
