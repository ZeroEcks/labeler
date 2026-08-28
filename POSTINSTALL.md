# Post Installation

## Set the Environment Variables

After installing, you will need to set the `STRIPE_SECRET_KEY`

```bash
export LABELER_APP_ID=000-aaa # you can get this from the cloudron web interface
# You need npm installed
npx cloudron env set --app $LABELER_APP_ID STRIPE_SECRET_KEY=sk_...
```
